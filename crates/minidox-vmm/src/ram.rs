use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cloud_hypervisor_hypervisor::Vm;

use crate::Error;

pub const RAM_PAGE_SIZE: usize = 4096;

static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct RamPage {
    id: u64,
    file: File,
}

impl RamPage {
    fn zeroed() -> Result<Arc<Self>, Error> {
        let file =
            tempfile::tempfile().map_err(|error| Error::backend("create RAM page", error))?;
        file.set_len(RAM_PAGE_SIZE as u64)
            .map_err(|error| Error::backend("size RAM page", error))?;
        Ok(Arc::new(Self {
            id: NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed),
            file,
        }))
    }

    fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>, Error> {
        let page = Self::zeroed()?;
        page.file
            .write_all_at(bytes, 0)
            .map_err(|error| Error::backend("write RAM page", error))?;
        Ok(page)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RamAccounting {
    pub resident_pages: usize,
    pub shared_pages: usize,
}

/// A contiguous KVM memory slot backed by page-granular immutable generations.
///
/// The first incarnation is shared with its page files. The first fork seals
/// those files and remaps the source and child privately. Later forks use the
/// KVM dirty bitmap plus host-write tracking to materialize only pages that
/// diverged since the preceding fork.
pub struct KvmGuestRam {
    base: NonNull<u8>,
    len: usize,
    pages: Vec<Arc<RamPage>>,
    host_dirty: BTreeSet<usize>,
    private: bool,
}

impl KvmGuestRam {
    pub fn new(len: usize) -> Result<Self, Error> {
        if len == 0 || !len.is_multiple_of(RAM_PAGE_SIZE) {
            return Err(Error::InvalidRamSize(len));
        }
        let base = reserve(len)?;
        let mut ram = Self {
            base,
            len,
            pages: Vec::with_capacity(len / RAM_PAGE_SIZE),
            host_dirty: BTreeSet::new(),
            private: false,
        };

        for index in 0..len / RAM_PAGE_SIZE {
            let page = RamPage::zeroed()?;
            ram.map_page(index, &page, false)?;
            ram.pages.push(page);
        }
        Ok(ram)
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn register(&self, vm: &dyn Vm, slot: u32, guest_phys_addr: u64) -> Result<(), Error> {
        // SAFETY: this mapping remains valid at the same virtual address until
        // the RAM branch is dropped; callers must unregister the slot first.
        unsafe {
            vm.create_user_memory_region(
                slot,
                guest_phys_addr,
                self.len,
                self.base.as_ptr(),
                false,
                true,
            )
        }
        .map_err(|error| Error::backend("register KVM RAM", error))?;
        if let Err(error) = vm.start_dirty_log() {
            // SAFETY: this exactly undoes the region installed above while the
            // backing mapping is still alive.
            let rollback = unsafe {
                vm.remove_user_memory_region(
                    slot,
                    guest_phys_addr,
                    self.len,
                    self.base.as_ptr(),
                    false,
                    true,
                )
            };
            if let Err(rollback_error) = rollback {
                return Err(Error::Backend {
                    operation: "start KVM dirty log",
                    message: format!("{error}; memory-slot rollback also failed: {rollback_error}"),
                });
            }
            return Err(Error::backend("start KVM dirty log", error));
        }
        Ok(())
    }

    pub fn unregister(&self, vm: &dyn Vm, slot: u32, guest_phys_addr: u64) -> Result<(), Error> {
        // SAFETY: the arguments exactly match the live slot installed by
        // `register`, and the mapping is still owned by this value.
        unsafe {
            vm.remove_user_memory_region(
                slot,
                guest_phys_addr,
                self.len,
                self.base.as_ptr(),
                false,
                true,
            )
        }
        .map_err(|error| Error::backend("unregister KVM RAM", error))
    }

    pub fn read(&self, offset: usize, bytes: &mut [u8]) -> Result<(), Error> {
        let range = self.checked_range(offset, bytes.len())?;
        // SAFETY: checked_range proves the source lies within the live mapping.
        let source =
            unsafe { slice::from_raw_parts(self.base.as_ptr().add(range.start), bytes.len()) };
        bytes.copy_from_slice(source);
        Ok(())
    }

    pub fn write(&mut self, offset: usize, bytes: &[u8]) -> Result<(), Error> {
        let range = self.checked_range(offset, bytes.len())?;
        // SAFETY: checked_range proves the destination lies within the live,
        // writable mapping, and `&mut self` excludes competing host writes.
        let target =
            unsafe { slice::from_raw_parts_mut(self.base.as_ptr().add(range.start), bytes.len()) };
        target.copy_from_slice(bytes);
        if !bytes.is_empty() {
            let first = range.start / RAM_PAGE_SIZE;
            let last = (range.end - 1) / RAM_PAGE_SIZE;
            self.host_dirty.extend(first..=last);
        }
        Ok(())
    }

    /// Fork after the VM has been paused and its vCPU has left `KVM_RUN`.
    pub fn fork(&mut self, vm: &dyn Vm, slot: u32, guest_phys_addr: u64) -> Result<Self, Error> {
        let bitmap = vm
            .get_dirty_log(slot, guest_phys_addr, self.len as u64)
            .map_err(|error| Error::backend("read KVM dirty log", error))?;
        let mut dirty = self.host_dirty.clone();
        for (word_index, word) in bitmap.into_iter().enumerate() {
            for bit in 0..64 {
                if word & (1_u64 << bit) != 0 {
                    let page = word_index * 64 + bit;
                    if page < self.pages.len() {
                        dirty.insert(page);
                    }
                }
            }
        }

        if self.private {
            for index in dirty {
                let page = RamPage::from_bytes(self.page_bytes(index))?;
                self.map_page(index, &page, true)?;
                self.pages[index] = page;
            }
        } else {
            for index in 0..self.pages.len() {
                let page = self.pages[index].clone();
                self.map_page(index, &page, true)?;
            }
            self.private = true;
        }
        self.host_dirty.clear();

        Self::from_pages(self.len, self.pages.clone())
    }

    pub fn page_accounting<'a>(branches: impl IntoIterator<Item = &'a Self>) -> RamAccounting {
        let mut references = BTreeMap::<u64, usize>::new();
        for branch in branches {
            for id in branch
                .pages
                .iter()
                .map(|page| page.id)
                .collect::<BTreeSet<_>>()
            {
                *references.entry(id).or_default() += 1;
            }
        }
        RamAccounting {
            resident_pages: references.len(),
            shared_pages: references.values().filter(|&&count| count > 1).count(),
        }
    }

    fn from_pages(len: usize, pages: Vec<Arc<RamPage>>) -> Result<Self, Error> {
        let base = reserve(len)?;
        let mut ram = Self {
            base,
            len,
            pages,
            host_dirty: BTreeSet::new(),
            private: true,
        };
        for index in 0..ram.pages.len() {
            let page = ram.pages[index].clone();
            ram.map_page(index, &page, true)?;
        }
        Ok(ram)
    }

    fn map_page(&mut self, index: usize, page: &RamPage, private: bool) -> Result<(), Error> {
        let address = self.base.as_ptr().wrapping_add(index * RAM_PAGE_SIZE);
        let flags = libc::MAP_FIXED
            | if private {
                libc::MAP_PRIVATE
            } else {
                libc::MAP_SHARED
            };
        // SAFETY: the destination is one page of the address range reserved by
        // this object, the fd is a page-sized regular file, and MAP_FIXED keeps
        // the KVM-visible virtual address stable.
        let mapped = unsafe {
            libc::mmap(
                address.cast(),
                RAM_PAGE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                page.file.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::backend("map RAM page", io::Error::last_os_error()));
        }
        Ok(())
    }

    fn page_bytes(&self, index: usize) -> &[u8] {
        // SAFETY: every page index comes from the page vector and is mapped.
        unsafe {
            slice::from_raw_parts(self.base.as_ptr().add(index * RAM_PAGE_SIZE), RAM_PAGE_SIZE)
        }
    }

    fn checked_range(&self, offset: usize, len: usize) -> Result<std::ops::Range<usize>, Error> {
        let end = offset
            .checked_add(len)
            .ok_or(Error::RamRange { offset, len })?;
        if end > self.len {
            return Err(Error::RamRange { offset, len });
        }
        Ok(offset..end)
    }
}

impl Drop for KvmGuestRam {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns the complete reserved mapping.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.len);
        }
    }
}

fn reserve(len: usize) -> Result<NonNull<u8>, Error> {
    // SAFETY: anonymous reservation with no supplied pointer or file descriptor.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(Error::backend(
            "reserve KVM RAM",
            io::Error::last_os_error(),
        ));
    }
    NonNull::new(mapped.cast())
        .ok_or_else(|| Error::backend("reserve KVM RAM", io::Error::other("mmap returned null")))
}
