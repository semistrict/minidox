//! Atomic memory and filesystem generation ownership for a fork forest.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use minidox_cache::{CacheBranchId, CacheError, NodeId, SharedPageCache};
use minidox_redoxfs::RedoxBranch;

pub const PAGE_SIZE: usize = 4096;

/// Stable identity of one VM in a fork forest.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct VmId(u64);

/// Physical-page accounting for one address space kind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpaceAccounting {
    pub resident_pages: usize,
    pub shared_pages: usize,
}

/// Physical pages reachable by all live VMs in a fork forest.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageAccounting {
    pub memory: SpaceAccounting,
    pub filesystem: SpaceAccounting,
}

#[derive(Debug)]
struct Page {
    id: u64,
    bytes: Box<[u8; PAGE_SIZE]>,
}

type FilePage = (String, u64);

#[derive(Debug)]
struct PageGeneration {
    parent: Option<Arc<Self>>,
    pages: BTreeMap<FilePage, Arc<Page>>,
}

impl PageGeneration {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            pages: BTreeMap::new(),
        })
    }

    fn page(&self, key: &FilePage) -> Option<&Arc<Page>> {
        self.pages
            .get(key)
            .or_else(|| self.parent.as_ref()?.page(key))
    }
}

#[derive(Debug)]
struct PageBranch {
    base: Arc<PageGeneration>,
    writes: BTreeMap<FilePage, Arc<Page>>,
}

impl PageBranch {
    fn empty() -> Self {
        Self {
            base: PageGeneration::empty(),
            writes: BTreeMap::new(),
        }
    }

    fn fork(&mut self) -> Self {
        if !self.writes.is_empty() {
            self.base = Arc::new(PageGeneration {
                parent: Some(Arc::clone(&self.base)),
                pages: std::mem::take(&mut self.writes),
            });
        }

        Self {
            base: Arc::clone(&self.base),
            writes: BTreeMap::new(),
        }
    }

    fn page(&self, key: &FilePage) -> Option<&Arc<Page>> {
        self.writes.get(key).or_else(|| self.base.page(key))
    }

    fn write(
        &mut self,
        next_page: &mut u64,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Error::RangeOverflow)?;
        let mut cursor = offset;

        while cursor < end {
            let page_number = cursor / PAGE_SIZE as u64;
            let page_offset = (cursor % PAGE_SIZE as u64) as usize;
            let count = (PAGE_SIZE - page_offset).min((end - cursor) as usize);
            let key = (path.to_owned(), page_number);
            let mut contents = self
                .page(&key)
                .map_or_else(|| Box::new([0; PAGE_SIZE]), |page| page.bytes.clone());
            let source_offset = (cursor - offset) as usize;
            contents[page_offset..page_offset + count]
                .copy_from_slice(&bytes[source_offset..source_offset + count]);

            let page = Arc::new(Page {
                id: *next_page,
                bytes: contents,
            });
            *next_page = next_page.checked_add(1).ok_or(Error::PageIdOverflow)?;
            self.writes.insert(key, page);
            cursor += count as u64;
        }

        Ok(())
    }

    fn read(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        let end = offset.checked_add(len as u64).ok_or(Error::RangeOverflow)?;
        let mut result = vec![0; len];
        let mut cursor = offset;

        while cursor < end {
            let page_number = cursor / PAGE_SIZE as u64;
            let page_offset = (cursor % PAGE_SIZE as u64) as usize;
            let count = (PAGE_SIZE - page_offset).min((end - cursor) as usize);
            let key = (path.to_owned(), page_number);
            if let Some(page) = self.page(&key) {
                let target_offset = (cursor - offset) as usize;
                result[target_offset..target_offset + count]
                    .copy_from_slice(&page.bytes[page_offset..page_offset + count]);
            }
            cursor += count as u64;
        }

        Ok(result)
    }

    fn visible_pages(&self) -> BTreeMap<FilePage, Arc<Page>> {
        let mut visible = self.writes.clone();
        let mut generation = Some(&*self.base);
        while let Some(current) = generation {
            for (key, page) in &current.pages {
                visible
                    .entry(key.clone())
                    .or_insert_with(|| Arc::clone(page));
            }
            generation = current.parent.as_deref();
        }
        visible
    }

    fn add_retained_page_ids(&self, retained: &mut BTreeSet<u64>) {
        retained.extend(self.writes.values().map(|page| page.id));
        let mut generation = Some(&*self.base);
        while let Some(current) = generation {
            retained.extend(current.pages.values().map(|page| page.id));
            generation = current.parent.as_deref();
        }
    }
}

#[derive(Debug)]
struct VmState {
    memory: PageBranch,
    filesystem: PageBranch,
}

/// Owns the VMs and immutable generations in one fork forest.
pub struct ForkForest {
    next_vm: u64,
    next_page: u64,
    vms: BTreeMap<VmId, VmState>,
}

impl ForkForest {
    pub fn new() -> Self {
        Self {
            next_vm: 1,
            next_page: 1,
            vms: BTreeMap::new(),
        }
    }

    pub fn create_vm(&mut self) -> VmId {
        let id = VmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            id,
            VmState {
                memory: PageBranch::empty(),
                filesystem: PageBranch::empty(),
            },
        );
        id
    }

    pub fn fork_vm(&mut self, source: VmId) -> Result<VmId, Error> {
        let source_vm = self.vms.get_mut(&source).ok_or(Error::VmNotFound(source))?;
        let child_memory = source_vm.memory.fork();
        let child_filesystem = source_vm.filesystem.fork();
        let child = VmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            child,
            VmState {
                memory: child_memory,
                filesystem: child_filesystem,
            },
        );
        Ok(child)
    }

    pub fn remove_vm(&mut self, vm: VmId) -> Result<(), Error> {
        self.vms
            .remove(&vm)
            .map(|_| ())
            .ok_or(Error::VmNotFound(vm))
    }

    pub fn write_file(
        &mut self,
        vm: VmId,
        path: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.vms
            .get_mut(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .filesystem
            .write(&mut self.next_page, path, offset, bytes)
    }

    pub fn read_file(
        &self,
        vm: VmId,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, Error> {
        self.vms
            .get(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .filesystem
            .read(path, offset, len)
    }

    pub fn write_memory(
        &mut self,
        vm: VmId,
        guest_address: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.vms
            .get_mut(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .memory
            .write(&mut self.next_page, "", guest_address, bytes)
    }

    pub fn read_memory(&self, vm: VmId, guest_address: u64, len: usize) -> Result<Vec<u8>, Error> {
        self.vms
            .get(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .memory
            .read("", guest_address, len)
    }

    pub fn page_accounting(&self) -> PageAccounting {
        PageAccounting {
            memory: account_pages(self.vms.values().map(|vm| &vm.memory)),
            filesystem: account_pages(self.vms.values().map(|vm| &vm.filesystem)),
        }
    }
}

fn account_pages<'a>(branches: impl Iterator<Item = &'a PageBranch>) -> SpaceAccounting {
    let mut resident = BTreeSet::new();
    let mut references = BTreeMap::<u64, usize>::new();
    for branch in branches {
        branch.add_retained_page_ids(&mut resident);
        let mut pages_in_vm = BTreeSet::new();
        for page in branch.visible_pages().into_values() {
            if pages_in_vm.insert(page.id) {
                *references.entry(page.id).or_default() += 1;
            }
        }
    }

    SpaceAccounting {
        resident_pages: resident.len(),
        shared_pages: references.values().filter(|count| **count > 1).count(),
    }
}

impl Default for ForkForest {
    fn default() -> Self {
        Self::new()
    }
}

struct SupervisedVm {
    memory: PageBranch,
    filesystem_cache: u64,
    filesystem_branch: CacheBranchId,
}

/// Owns the RAM and RedoxFS/DAX branches for a fork forest.
pub struct Supervisor {
    next_vm: u64,
    next_page: u64,
    vms: BTreeMap<VmId, SupervisedVm>,
    filesystems: BTreeMap<u64, Arc<SharedPageCache<RedoxBranch>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            next_vm: 1,
            next_page: 1,
            vms: BTreeMap::new(),
            filesystems: BTreeMap::new(),
        }
    }

    pub fn create_vm(&mut self) -> Result<VmId, Error> {
        let id = VmId(self.next_vm);
        self.next_vm += 1;
        let filesystem = RedoxBranch::create(32 * 1024 * 1024)?;
        let (cache, branch) = SharedPageCache::new(filesystem);
        let cache = Arc::new(cache);
        let cache_id = cache.id();
        self.filesystems.insert(cache_id, cache);
        self.vms.insert(
            id,
            SupervisedVm {
                memory: PageBranch::empty(),
                filesystem_cache: cache_id,
                filesystem_branch: branch,
            },
        );
        Ok(id)
    }

    pub fn create_file(&mut self, vm: VmId, name: &str, size: u64) -> Result<NodeId, Error> {
        let vm = self.vms.get(&vm).ok_or(Error::VmNotFound(vm))?;
        let cache = self.filesystems[&vm.filesystem_cache].clone();
        cache
            .with_store_mut(vm.filesystem_branch, |store| store.create_file(name, size))
            .map_err(Into::into)
    }

    /// Publish one fork point for the source's filesystem cache and RAM.
    pub fn fork_vm(&mut self, source: VmId) -> Result<VmId, Error> {
        let source_vm = self.vms.get_mut(&source).ok_or(Error::VmNotFound(source))?;
        let filesystem_cache = source_vm.filesystem_cache;
        let child_filesystem =
            self.filesystems[&filesystem_cache].fork(source_vm.filesystem_branch)?;
        let child_memory = source_vm.memory.fork();
        let child = VmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            child,
            SupervisedVm {
                memory: child_memory,
                filesystem_cache,
                filesystem_branch: child_filesystem,
            },
        );
        Ok(child)
    }

    pub fn remove_vm(&mut self, vm: VmId) -> Result<(), Error> {
        let removed = self.vms.remove(&vm).ok_or(Error::VmNotFound(vm))?;
        let cache = self.filesystems[&removed.filesystem_cache].clone();
        cache.remove_branch(removed.filesystem_branch)?;
        if !self
            .vms
            .values()
            .any(|vm| vm.filesystem_cache == removed.filesystem_cache)
        {
            self.filesystems.remove(&removed.filesystem_cache);
        }
        Ok(())
    }

    pub fn write_file(
        &mut self,
        vm: VmId,
        node: NodeId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        let vm = self.vms.get(&vm).ok_or(Error::VmNotFound(vm))?;
        let cache = self.filesystems[&vm.filesystem_cache].clone();
        let branch = vm.filesystem_branch;
        cache.open(branch, node)?;
        let result = cache.write(branch, node, offset, bytes);
        let close_result = cache.close(branch, node);
        result?;
        close_result?;
        Ok(())
    }

    pub fn read_file(
        &mut self,
        vm: VmId,
        node: NodeId,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, Error> {
        let vm = self.vms.get(&vm).ok_or(Error::VmNotFound(vm))?;
        let cache = self.filesystems[&vm.filesystem_cache].clone();
        let branch = vm.filesystem_branch;
        cache.open(branch, node)?;
        let mut bytes = vec![0; len];
        let result = cache.read(branch, node, offset, &mut bytes);
        let close_result = cache.close(branch, node);
        let read = result?;
        close_result?;
        bytes.truncate(read);
        Ok(bytes)
    }

    pub fn write_memory(
        &mut self,
        vm: VmId,
        guest_address: u64,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.vms
            .get_mut(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .memory
            .write(&mut self.next_page, "", guest_address, bytes)
    }

    pub fn read_memory(&self, vm: VmId, guest_address: u64, len: usize) -> Result<Vec<u8>, Error> {
        self.vms
            .get(&vm)
            .ok_or(Error::VmNotFound(vm))?
            .memory
            .read("", guest_address, len)
    }

    pub fn page_accounting(&self) -> PageAccounting {
        let mut filesystem = SpaceAccounting::default();
        for (&cache_id, cache) in &self.filesystems {
            let branches = self
                .vms
                .values()
                .filter(|vm| vm.filesystem_cache == cache_id)
                .map(|vm| vm.filesystem_branch);
            let accounting = cache
                .page_accounting(branches)
                .expect("supervisor owns every referenced filesystem branch");
            filesystem.resident_pages += accounting.resident_pages;
            filesystem.shared_pages += accounting.shared_pages;
        }
        PageAccounting {
            memory: account_pages(self.vms.values().map(|vm| &vm.memory)),
            filesystem,
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VM {0:?} does not exist")]
    VmNotFound(VmId),
    #[error("page range overflows the address space")]
    RangeOverflow,
    #[error("physical page identity space is exhausted")]
    PageIdOverflow,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Cache(#[from] CacheError),
}
