use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cloud_hypervisor_hypervisor::Vm;

use crate::Error;

pub const RAM_PAGE_SIZE: usize = 4096;

static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_GENERATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct RamGeneration {
    id: u64,
    file: File,
}

impl RamGeneration {
    fn zeroed(len: usize) -> Result<Arc<Self>, Error> {
        let file =
            tempfile::tempfile().map_err(|error| Error::backend("create RAM generation", error))?;
        file.set_len(len as u64)
            .map_err(|error| Error::backend("size RAM generation", error))?;
        Ok(Arc::new(Self {
            id: NEXT_GENERATION_ID.fetch_add(1, Ordering::Relaxed),
            file,
        }))
    }
}

#[derive(Debug)]
struct RamPage {
    id: u64,
    generation: Arc<RamGeneration>,
    offset: u64,
}

impl RamPage {
    fn new(generation: Arc<RamGeneration>, offset: u64) -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed),
            generation,
            offset,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RamAccounting {
    pub resident_pages: usize,
    pub shared_pages: usize,
    pub backing_files: usize,
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
    sealed: bool,
}

pub(crate) struct RamForkPreparation {
    generation: Arc<RamGeneration>,
    pages: BTreeMap<usize, Arc<RamPage>>,
    seals_existing_generation: bool,
}

pub(crate) struct RamForkBranch {
    len: usize,
    pages: Vec<Arc<RamPage>>,
}

impl RamForkBranch {
    pub(crate) fn materialize(self) -> Result<KvmGuestRam, Error> {
        KvmGuestRam::from_pages(self.len, self.pages)
    }
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
            private: true,
            sealed: false,
        };

        let generation = RamGeneration::zeroed(len)?;
        for index in 0..len / RAM_PAGE_SIZE {
            let page = RamPage::new(Arc::clone(&generation), (index * RAM_PAGE_SIZE) as u64);
            ram.pages.push(page);
        }
        ram.map_all_pages(true)?;
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

        self.fork_pages(dirty)
    }

    /// Fork using dirty host-page indexes captured by the embedded VMM.
    pub fn fork_dirty_pages(&mut self, pages: &[usize]) -> Result<Self, Error> {
        let mut dirty = self.host_dirty.clone();
        for &page in pages {
            if page >= self.pages.len() {
                return Err(Error::InvalidDirtyPage {
                    page,
                    pages: self.pages.len(),
                });
            }
            dirty.insert(page);
        }
        self.fork_pages(dirty)
    }

    /// Copy the current dirty set while the VM keeps running. Writes racing
    /// these copies remain tracked by KVM and are recopied at the pause barrier.
    pub(crate) fn prepare_fork(&self, pages: &[usize]) -> Result<RamForkPreparation, Error> {
        let pages = pages.iter().copied().collect::<BTreeSet<_>>();
        for &page in &pages {
            if page >= self.pages.len() {
                return Err(Error::InvalidDirtyPage {
                    page,
                    pages: self.pages.len(),
                });
            }
        }

        if !self.sealed {
            for &page in &pages {
                let backing = &self.pages[page];
                self.copy_page_to(page, &backing.generation.file, backing.offset)?;
            }
            return Ok(RamForkPreparation {
                generation: Arc::clone(&self.pages[0].generation),
                pages: BTreeMap::new(),
                seals_existing_generation: true,
            });
        }

        let generation = RamGeneration::zeroed(pages.len() * RAM_PAGE_SIZE)?;
        let mut prepared = BTreeMap::new();
        for (generation_index, page) in pages.into_iter().enumerate() {
            let offset = (generation_index * RAM_PAGE_SIZE) as u64;
            self.copy_page_to(page, &generation.file, offset)?;
            prepared.insert(page, RamPage::new(Arc::clone(&generation), offset));
        }
        Ok(RamForkPreparation {
            generation,
            pages: prepared,
            seals_existing_generation: false,
        })
    }

    pub(crate) fn finish_prepared_branch(
        &mut self,
        preparation: RamForkPreparation,
        pages_dirtied_during_copy: &[usize],
    ) -> Result<RamForkBranch, Error> {
        let mut dirty = self.host_dirty.clone();
        for &page in pages_dirtied_during_copy {
            if page >= self.pages.len() {
                return Err(Error::InvalidDirtyPage {
                    page,
                    pages: self.pages.len(),
                });
            }
            dirty.insert(page);
        }
        self.finish_prepared_pages(preparation, dirty)
    }

    fn fork_pages(&mut self, dirty: BTreeSet<usize>) -> Result<Self, Error> {
        let preparation = self.prepare_fork(&[])?;
        self.finish_prepared_pages(preparation, dirty)?
            .materialize()
    }

    fn finish_prepared_pages(
        &mut self,
        mut preparation: RamForkPreparation,
        dirty: BTreeSet<usize>,
    ) -> Result<RamForkBranch, Error> {
        if preparation.seals_existing_generation {
            for index in dirty {
                let backing = &self.pages[index];
                self.copy_page_to(index, &backing.generation.file, backing.offset)?;
            }
            self.discard_private_pages()?;
            self.private = true;
            self.sealed = true;
            self.host_dirty.clear();
            return Ok(RamForkBranch {
                len: self.len,
                pages: self.pages.clone(),
            });
        }

        if self.private {
            let mut next_offset = preparation.pages.len() * RAM_PAGE_SIZE;
            for index in dirty {
                let page = if let Some(page) = preparation.pages.get(&index) {
                    Arc::clone(page)
                } else {
                    let offset = next_offset as u64;
                    next_offset += RAM_PAGE_SIZE;
                    preparation
                        .generation
                        .file
                        .set_len(next_offset as u64)
                        .map_err(|error| Error::backend("grow RAM generation", error))?;
                    let page = RamPage::new(Arc::clone(&preparation.generation), offset);
                    preparation.pages.insert(index, Arc::clone(&page));
                    page
                };
                self.copy_page_to(index, &preparation.generation.file, page.offset)?;
            }

            let changed = preparation.pages.keys().copied().collect::<BTreeSet<_>>();
            for (index, page) in preparation.pages {
                self.pages[index] = page;
            }
            self.map_selected_pages(&changed, true)?;
        } else {
            self.map_all_pages(true)?;
            self.private = true;
        }
        self.sealed = true;
        self.host_dirty.clear();

        Ok(RamForkBranch {
            len: self.len,
            pages: self.pages.clone(),
        })
    }

    pub fn page_accounting<'a>(branches: impl IntoIterator<Item = &'a Self>) -> RamAccounting {
        let mut references = BTreeMap::<u64, usize>::new();
        let mut backing_files = BTreeSet::new();
        for branch in branches {
            for page in &branch.pages {
                backing_files.insert(page.generation.id);
            }
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
            backing_files: backing_files.len(),
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
            sealed: true,
        };
        ram.map_all_pages(true)?;
        Ok(ram)
    }

    fn map_all_pages(&mut self, private: bool) -> Result<(), Error> {
        let mut start = 0;
        while start < self.pages.len() {
            let generation = Arc::clone(&self.pages[start].generation);
            let generation_offset = self.pages[start].offset;
            let mut end = start + 1;
            while end < self.pages.len()
                && self.pages[end].generation.id == generation.id
                && self.pages[end].offset
                    == generation_offset + ((end - start) * RAM_PAGE_SIZE) as u64
            {
                end += 1;
            }
            self.map_run(start, end - start, &generation, generation_offset, private)?;
            start = end;
        }
        Ok(())
    }

    fn map_selected_pages(
        &mut self,
        selected: &BTreeSet<usize>,
        private: bool,
    ) -> Result<(), Error> {
        let mut selected = selected.iter().copied().peekable();
        while let Some(start) = selected.next() {
            let generation = Arc::clone(&self.pages[start].generation);
            let generation_offset = self.pages[start].offset;
            let mut end = start + 1;
            while selected.peek().copied() == Some(end)
                && self.pages[end].generation.id == generation.id
                && self.pages[end].offset
                    == generation_offset + ((end - start) * RAM_PAGE_SIZE) as u64
            {
                selected.next();
                end += 1;
            }
            self.map_run(start, end - start, &generation, generation_offset, private)?;
        }
        Ok(())
    }

    fn map_run(
        &mut self,
        start_page: usize,
        page_count: usize,
        generation: &RamGeneration,
        generation_offset: u64,
        private: bool,
    ) -> Result<(), Error> {
        let address = self.base.as_ptr().wrapping_add(start_page * RAM_PAGE_SIZE);
        let flags = libc::MAP_FIXED
            | if private {
                libc::MAP_PRIVATE
            } else {
                libc::MAP_SHARED
            };
        // SAFETY: the destination is a page-aligned subrange of the reservation,
        // the generation contains the complete file range, and MAP_FIXED keeps
        // the KVM-visible virtual address stable.
        let mapped = unsafe {
            libc::mmap(
                address.cast(),
                page_count * RAM_PAGE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                generation.file.as_raw_fd(),
                generation_offset as libc::off_t,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::backend("map RAM run", io::Error::last_os_error()));
        }
        Ok(())
    }

    fn discard_private_pages(&mut self) -> Result<(), Error> {
        // SAFETY: the full range is a live MAP_PRIVATE file mapping owned by
        // this RAM object. The VM is paused, and the backing file now contains
        // the exact fork-point bytes, so discarding private CoW pages makes the
        // source and child fault the same page-cache pages without moving the
        // KVM-visible virtual address.
        let result =
            unsafe { libc::madvise(self.base.as_ptr().cast(), self.len, libc::MADV_DONTNEED) };
        if result != 0 {
            return Err(Error::backend(
                "discard sealed private RAM pages",
                io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    fn copy_page_to(&self, index: usize, file: &File, offset: u64) -> Result<(), Error> {
        let source = self.base.as_ptr().wrapping_add(index * RAM_PAGE_SIZE);
        let mut written = 0;
        while written < RAM_PAGE_SIZE {
            // SAFETY: the source points into the live RAM mapping. KVM may
            // mutate it concurrently during preparation; a racing page is
            // logged again and recopied after vCPUs stop.
            let result = unsafe {
                libc::pwrite(
                    file.as_raw_fd(),
                    source.add(written).cast(),
                    RAM_PAGE_SIZE - written,
                    (offset + written as u64) as libc::off_t,
                )
            };
            if result < 0 {
                return Err(Error::backend(
                    "copy RAM generation page",
                    io::Error::last_os_error(),
                ));
            }
            if result == 0 {
                return Err(Error::backend(
                    "copy RAM generation page",
                    io::Error::new(io::ErrorKind::WriteZero, "pwrite returned zero"),
                ));
            }
            written += result as usize;
        }
        Ok(())
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
