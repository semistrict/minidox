use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    CacheError, MapFlags, NodeId, NodeStore, PAGE_SIZE, Result, read_exact_at, write_all_at,
};

static NEXT_CACHE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);

pub trait ForkableNodeStore: NodeStore + Sized {
    type PageIdentity: Clone + Ord;

    fn page_identity(&mut self, node: NodeId, offset: u64) -> io::Result<Self::PageIdentity>;
    fn prepare_page_write(&mut self, node: NodeId, offset: u64) -> io::Result<Self::PageIdentity>;
    fn fork_store(&mut self) -> io::Result<Self>;
}

#[derive(Debug)]
struct CachePage {
    id: u64,
    backing: Arc<File>,
}

impl CachePage {
    fn from_bytes(bytes: &[u8; PAGE_SIZE as usize]) -> io::Result<Arc<Self>> {
        let backing = tempfile::tempfile()?;
        backing.set_len(PAGE_SIZE)?;
        write_all_at(&backing, bytes, 0)?;
        Ok(Arc::new(Self {
            id: NEXT_PAGE_ID.fetch_add(1, Ordering::Relaxed),
            backing: Arc::new(backing),
        }))
    }

    fn duplicate(&self) -> io::Result<Arc<Self>> {
        let mut bytes = [0; PAGE_SIZE as usize];
        read_exact_at(&self.backing, &mut bytes, 0)?;
        Self::from_bytes(&bytes)
    }
}

type PageKey = (NodeId, u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CacheBranchId(u64);

#[derive(Debug)]
pub struct DaxPageMapping {
    cache_id: u64,
    branch: CacheBranchId,
    key: PageKey,
    flags: MapFlags,
    page: Arc<CachePage>,
}

impl DaxPageMapping {
    pub fn node(&self) -> NodeId {
        self.key.0
    }

    pub fn file_offset(&self) -> u64 {
        self.key.1
    }

    pub fn flags(&self) -> MapFlags {
        self.flags
    }

    pub fn page_id(&self) -> u64 {
        self.page.id
    }

    pub fn backing(&self) -> &File {
        &self.page.backing
    }

    pub fn try_clone_backing(&self) -> io::Result<File> {
        self.page.backing.try_clone()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CachePageAccounting {
    pub resident_pages: usize,
    pub shared_pages: usize,
}

struct CacheBranch<S> {
    store: S,
    open_nodes: BTreeMap<NodeId, usize>,
    active_pages: BTreeMap<u64, usize>,
    exclusive_pages: BTreeSet<PageKey>,
    dirty_pages: DirtyBranch,
}

#[derive(Debug, Default)]
struct DirtyGeneration {
    parent: Option<Arc<DirtyGeneration>>,
    changes: BTreeMap<PageKey, bool>,
}

struct DirtyBranch {
    base: Arc<DirtyGeneration>,
    changes: BTreeMap<PageKey, bool>,
}

impl DirtyBranch {
    fn new() -> Self {
        Self {
            base: Arc::new(DirtyGeneration::default()),
            changes: BTreeMap::new(),
        }
    }

    fn fork(&mut self) -> Self {
        if !self.changes.is_empty() {
            self.base = Arc::new(DirtyGeneration {
                parent: Some(Arc::clone(&self.base)),
                changes: std::mem::take(&mut self.changes),
            });
        }
        Self {
            base: Arc::clone(&self.base),
            changes: BTreeMap::new(),
        }
    }

    fn mark_dirty(&mut self, key: PageKey) {
        self.changes.insert(key, true);
    }

    fn mark_clean(&mut self, key: PageKey) {
        self.changes.insert(key, false);
    }

    fn pages_for_node(&self, node: NodeId) -> Vec<PageKey> {
        let mut visible = BTreeMap::new();
        for (&key, &dirty) in &self.changes {
            if key.0 == node {
                visible.insert(key, dirty);
            }
        }
        let mut generation = Some(&*self.base);
        while let Some(current) = generation {
            for (&key, &dirty) in &current.changes {
                if key.0 == node {
                    visible.entry(key).or_insert(dirty);
                }
            }
            generation = current.parent.as_deref();
        }
        visible
            .into_iter()
            .filter_map(|(key, dirty)| dirty.then_some(key))
            .collect()
    }
}

struct State<S: ForkableNodeStore> {
    next_branch: u64,
    branches: BTreeMap<CacheBranchId, CacheBranch<S>>,
    pages: BTreeMap<S::PageIdentity, Arc<CachePage>>,
    known_pages: BTreeSet<PageKey>,
}

/// The one logical file-page cache owned by a filesystem lineage.
///
/// VMs hold only [`CacheBranchId`] capabilities. All branch-visible versions
/// resolve through this cache, so a cold fault of one unchanged page produces
/// one backing object regardless of which descendant faults it first.
pub struct SharedPageCache<S: ForkableNodeStore> {
    id: u64,
    state: Mutex<State<S>>,
}

impl<S: ForkableNodeStore> SharedPageCache<S> {
    pub fn new(store: S) -> (Self, CacheBranchId) {
        let branch = CacheBranchId(1);
        let mut branches = BTreeMap::new();
        branches.insert(
            branch,
            CacheBranch {
                store,
                open_nodes: BTreeMap::new(),
                active_pages: BTreeMap::new(),
                exclusive_pages: BTreeSet::new(),
                dirty_pages: DirtyBranch::new(),
            },
        );
        (
            Self {
                id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
                state: Mutex::new(State {
                    next_branch: 2,
                    branches,
                    pages: BTreeMap::new(),
                    known_pages: BTreeSet::new(),
                }),
            },
            branch,
        )
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn with_store_mut<T>(
        &self,
        branch: CacheBranchId,
        operation: impl FnOnce(&mut S) -> io::Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        let store = &mut branch_mut(&mut state, branch)?.store;
        Ok(operation(store)?)
    }

    pub fn fork(&self, source: CacheBranchId) -> Result<CacheBranchId> {
        let mut state = self.state.lock().unwrap();
        let child_store;
        let dirty_pages;
        {
            let source_branch = branch_mut(&mut state, source)?;
            if !source_branch.active_pages.is_empty() {
                return Err(CacheError::ActiveMappings);
            }
            child_store = source_branch.store.fork_store()?;
            dirty_pages = source_branch.dirty_pages.fork();
            source_branch.exclusive_pages.clear();
        }
        let child = CacheBranchId(state.next_branch);
        state.next_branch = state
            .next_branch
            .checked_add(1)
            .ok_or(CacheError::InvalidMapping("cache branch identity overflow"))?;
        state.branches.insert(
            child,
            CacheBranch {
                store: child_store,
                open_nodes: BTreeMap::new(),
                active_pages: BTreeMap::new(),
                exclusive_pages: BTreeSet::new(),
                dirty_pages,
            },
        );
        Ok(child)
    }

    pub fn remove_branch(&self, branch: CacheBranchId) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let branch_state = state
            .branches
            .get(&branch)
            .ok_or(CacheError::BranchNotFound(branch.0))?;
        if !branch_state.active_pages.is_empty() {
            return Err(CacheError::ActiveMappings);
        }
        state.branches.remove(&branch);
        let mut visible = BTreeSet::new();
        let known_pages = state.known_pages.iter().copied().collect::<Vec<_>>();
        for (node, offset) in known_pages {
            for branch in state.branches.values_mut() {
                if let Ok(identity) = branch.store.page_identity(node, offset) {
                    visible.insert(identity);
                }
            }
        }
        state.pages.retain(|identity, _| visible.contains(identity));
        Ok(())
    }

    pub fn open(&self, branch: CacheBranchId, node: NodeId) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let branch = branch_mut(&mut state, branch)?;
        let count = branch.open_nodes.entry(node).or_default();
        if *count == 0 {
            branch.store.on_open_node(node)?;
        }
        *count = count
            .checked_add(1)
            .ok_or(CacheError::InvalidMapping("open reference count overflow"))?;
        Ok(())
    }

    pub fn close(&self, branch: CacheBranchId, node: NodeId) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let branch = branch_mut(&mut state, branch)?;
        let count = branch
            .open_nodes
            .get_mut(&node)
            .ok_or(CacheError::NodeNotOpen(node))?;
        *count = count
            .checked_sub(1)
            .ok_or(CacheError::InvalidMapping("node is not open"))?;
        if *count == 0 {
            branch.store.on_close_node(node)?;
        }
        Ok(())
    }

    pub fn map_page(
        &self,
        branch_id: CacheBranchId,
        node: NodeId,
        offset: u64,
        flags: MapFlags,
    ) -> Result<DaxPageMapping> {
        if offset % PAGE_SIZE != 0 {
            return Err(CacheError::InvalidMapping(
                "mapping offset must be page aligned",
            ));
        }
        if !flags.intersects(MapFlags::READ | MapFlags::WRITE) {
            return Err(CacheError::InvalidMapping(
                "mapping must be readable or writable",
            ));
        }

        let key = (node, offset);
        let mut state = self.state.lock().unwrap();
        let State {
            branches,
            pages,
            known_pages,
            ..
        } = &mut *state;
        let (identity, mut page, copy_on_write) = {
            let branch = branches
                .get_mut(&branch_id)
                .ok_or(CacheError::BranchNotFound(branch_id.0))?;
            if branch.open_nodes.get(&node).copied().unwrap_or(0) == 0 {
                return Err(CacheError::NodeNotOpen(node));
            }

            let identity = branch.store.page_identity(node, offset)?;
            let page = load_page(pages, identity.clone(), &mut branch.store, node, offset)?;
            let copy_on_write =
                flags.contains(MapFlags::WRITE) && !branch.exclusive_pages.contains(&key);
            if copy_on_write && branch.active_pages.get(&page.id).copied().unwrap_or(0) > 0 {
                return Err(CacheError::PageBusy);
            }
            (identity, page, copy_on_write)
        };
        if copy_on_write {
            let new_page = page.duplicate()?;
            let new_identity = branches
                .get_mut(&branch_id)
                .expect("branch was resolved above")
                .store
                .prepare_page_write(node, offset)?;
            page = new_page;
            pages.insert(new_identity, Arc::clone(&page));
            let old_identity_is_visible = branches.values_mut().any(|branch| {
                branch.store.page_identity(node, offset).ok().as_ref() == Some(&identity)
            });
            if !old_identity_is_visible {
                pages.remove(&identity);
            }
            branches
                .get_mut(&branch_id)
                .expect("branch was resolved above")
                .exclusive_pages
                .insert(key);
        }

        known_pages.insert(key);
        let branch = branches
            .get_mut(&branch_id)
            .expect("branch was resolved above");
        *branch.active_pages.entry(page.id).or_default() += 1;
        Ok(DaxPageMapping {
            cache_id: self.id,
            branch: branch_id,
            key,
            flags,
            page,
        })
    }

    pub fn unmap(&self, branch_id: CacheBranchId, mapping: DaxPageMapping) -> Result<()> {
        if mapping.cache_id != self.id || mapping.branch != branch_id {
            return Err(CacheError::StaleMapping);
        }
        let mut state = self.state.lock().unwrap();
        let branch = branch_mut(&mut state, branch_id)?;
        let count = branch
            .active_pages
            .get_mut(&mapping.page.id)
            .ok_or(CacheError::StaleMapping)?;
        *count = count.checked_sub(1).ok_or(CacheError::StaleMapping)?;
        if *count == 0 {
            branch.active_pages.remove(&mapping.page.id);
        }
        if mapping.flags.contains(MapFlags::WRITE) {
            branch.dirty_pages.mark_dirty(mapping.key);
        }
        Ok(())
    }

    pub fn read(
        &self,
        branch: CacheBranchId,
        node: NodeId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        let file_len = self.with_store_mut(branch, |store| store.len(node))?;
        if offset >= file_len || buf.is_empty() {
            return Ok(0);
        }
        let count = usize::try_from((file_len - offset).min(buf.len() as u64))
            .map_err(|_| CacheError::InvalidMapping("read length does not fit usize"))?;
        let end = offset
            .checked_add(count as u64)
            .ok_or(CacheError::InvalidMapping("read range overflow"))?;
        let mut cursor = offset;
        while cursor < end {
            let page_offset = cursor / PAGE_SIZE * PAGE_SIZE;
            let within_page = cursor - page_offset;
            let chunk_len = usize::try_from((PAGE_SIZE - within_page).min(end - cursor))
                .map_err(|_| CacheError::InvalidMapping("read length does not fit usize"))?;
            let target_offset = usize::try_from(cursor - offset)
                .map_err(|_| CacheError::InvalidMapping("read offset does not fit usize"))?;
            let mapping = self.map_page(branch, node, page_offset, MapFlags::READ)?;
            mapping.backing().read_exact_at(
                &mut buf[target_offset..target_offset + chunk_len],
                within_page,
            )?;
            self.unmap(branch, mapping)?;
            cursor += chunk_len as u64;
        }
        Ok(count)
    }

    pub fn write(
        &self,
        branch: CacheBranchId,
        node: NodeId,
        offset: u64,
        buf: &[u8],
    ) -> Result<usize> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(CacheError::InvalidMapping("write range overflow"))?;
        let file_len = self.with_store_mut(branch, |store| store.len(node))?;
        if end > file_len {
            return Err(CacheError::InvalidMapping("write extends beyond file"));
        }
        let mut cursor = offset;
        while cursor < end {
            let page_offset = cursor / PAGE_SIZE * PAGE_SIZE;
            let within_page = cursor - page_offset;
            let chunk_len = usize::try_from((PAGE_SIZE - within_page).min(end - cursor))
                .map_err(|_| CacheError::InvalidMapping("write length does not fit usize"))?;
            let source_offset = usize::try_from(cursor - offset)
                .map_err(|_| CacheError::InvalidMapping("write offset does not fit usize"))?;
            let mapping =
                self.map_page(branch, node, page_offset, MapFlags::READ | MapFlags::WRITE)?;
            mapping
                .backing()
                .write_all_at(&buf[source_offset..source_offset + chunk_len], within_page)?;
            self.unmap(branch, mapping)?;
            cursor += chunk_len as u64;
        }
        Ok(buf.len())
    }

    pub fn sync(&self, branch_id: CacheBranchId, node: NodeId) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let State {
            branches, pages, ..
        } = &mut *state;
        let branch = branches
            .get_mut(&branch_id)
            .ok_or(CacheError::BranchNotFound(branch_id.0))?;
        let dirty_pages = branch.dirty_pages.pages_for_node(node);
        let file_len = branch.store.len(node)?;
        for key in dirty_pages {
            if key.1 >= file_len {
                branch.dirty_pages.mark_clean(key);
                continue;
            }
            let identity = branch.store.page_identity(node, key.1)?;
            let page = pages.get(&identity).ok_or(CacheError::StaleMapping)?;
            let count = usize::try_from((file_len - key.1).min(PAGE_SIZE))
                .map_err(|_| CacheError::InvalidMapping("page length does not fit usize"))?;
            let mut bytes = [0; PAGE_SIZE as usize];
            read_exact_at(&page.backing, &mut bytes[..count], 0)?;
            let written = branch.store.write_node(node, key.1, &bytes[..count])?;
            if written != count {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "short filesystem write").into(),
                );
            }
            branch.dirty_pages.mark_clean(key);
        }
        Ok(())
    }

    pub fn page_accounting(
        &self,
        branches: impl IntoIterator<Item = CacheBranchId>,
    ) -> Result<CachePageAccounting> {
        let requested = branches.into_iter().collect::<Vec<_>>();
        let mut state = self.state.lock().unwrap();
        let State {
            branches,
            pages,
            known_pages,
            ..
        } = &mut *state;
        let mut references = BTreeMap::<u64, usize>::new();
        for branch_id in requested {
            let branch = branches
                .get_mut(&branch_id)
                .ok_or(CacheError::BranchNotFound(branch_id.0))?;
            let mut pages_in_branch = BTreeSet::new();
            for &(node, offset) in known_pages.iter() {
                let identity = match branch.store.page_identity(node, offset) {
                    Ok(identity) => identity,
                    Err(_) => continue,
                };
                if let Some(page) = pages.get(&identity) {
                    pages_in_branch.insert(page.id);
                }
            }
            for page in pages_in_branch {
                *references.entry(page).or_default() += 1;
            }
        }
        Ok(CachePageAccounting {
            resident_pages: references.len(),
            shared_pages: references.values().filter(|&&count| count > 1).count(),
        })
    }
}

fn load_page<S: ForkableNodeStore>(
    pages: &mut BTreeMap<S::PageIdentity, Arc<CachePage>>,
    identity: S::PageIdentity,
    store: &mut S,
    node: NodeId,
    offset: u64,
) -> Result<Arc<CachePage>> {
    if let Some(page) = pages.get(&identity) {
        return Ok(Arc::clone(page));
    }
    let mut bytes = [0; PAGE_SIZE as usize];
    let read = store.read_node(node, offset, &mut bytes)?;
    bytes[read..].fill(0);
    let page = CachePage::from_bytes(&bytes)?;
    pages.insert(identity, Arc::clone(&page));
    Ok(page)
}

fn branch_mut<S: ForkableNodeStore>(
    state: &mut State<S>,
    branch: CacheBranchId,
) -> Result<&mut CacheBranch<S>> {
    state
        .branches
        .get_mut(&branch)
        .ok_or(CacheError::BranchNotFound(branch.0))
}
