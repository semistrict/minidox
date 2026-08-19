use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    CacheError, MapFlags, NodeId, NodeStore, PAGE_SIZE, Result, read_exact_at, write_all_at,
};

static NEXT_CACHE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PAGE_ID: AtomicU64 = AtomicU64::new(1);

pub trait ForkableNodeStore: NodeStore + Sized {
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

#[derive(Clone, Debug)]
struct PageEntry {
    page: Arc<CachePage>,
    dirty: bool,
}

type PageKey = (NodeId, u64);

#[derive(Debug, Default)]
struct PageGeneration {
    parent: Option<Arc<PageGeneration>>,
    pages: BTreeMap<PageKey, PageEntry>,
}

impl PageGeneration {
    fn entry(&self, key: &PageKey) -> Option<&PageEntry> {
        self.pages
            .get(key)
            .or_else(|| self.parent.as_deref().and_then(|parent| parent.entry(key)))
    }

    fn visible_entries(&self, entries: &mut BTreeMap<PageKey, PageEntry>) {
        if let Some(parent) = &self.parent {
            parent.visible_entries(entries);
        }
        entries.extend(self.pages.iter().map(|(key, entry)| (*key, entry.clone())));
    }
}

#[derive(Debug)]
struct PageBranch {
    base: Arc<PageGeneration>,
    writes: BTreeMap<PageKey, PageEntry>,
}

impl PageBranch {
    fn new() -> Self {
        Self {
            base: Arc::new(PageGeneration::default()),
            writes: BTreeMap::new(),
        }
    }

    fn fork(&mut self) -> Self {
        if !self.writes.is_empty() {
            self.base = Arc::new(PageGeneration {
                parent: Some(self.base.clone()),
                pages: std::mem::take(&mut self.writes),
            });
        }
        Self {
            base: self.base.clone(),
            writes: BTreeMap::new(),
        }
    }

    fn entry(&self, key: &PageKey) -> Option<&PageEntry> {
        self.writes.get(key).or_else(|| self.base.entry(key))
    }

    fn visible_entries(&self) -> BTreeMap<PageKey, PageEntry> {
        let mut entries = BTreeMap::new();
        self.base.visible_entries(&mut entries);
        entries.extend(self.writes.iter().map(|(key, entry)| (*key, entry.clone())));
        entries
    }
}

#[derive(Debug)]
pub struct DaxPageMapping {
    cache_id: u64,
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

/// A page-granular DAX cache whose immutable pages follow filesystem forks.
pub struct BranchingPageCache<S> {
    id: u64,
    store: S,
    pages: PageBranch,
    open_nodes: BTreeMap<NodeId, usize>,
    active_pages: BTreeMap<u64, usize>,
}

impl<S: NodeStore> BranchingPageCache<S> {
    pub fn new(store: S) -> Self {
        Self {
            id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
            store,
            pages: PageBranch::new(),
            open_nodes: BTreeMap::new(),
            active_pages: BTreeMap::new(),
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn open(&mut self, node: NodeId) -> Result<()> {
        let count = self.open_nodes.entry(node).or_default();
        if *count == 0 {
            self.store.on_open_node(node)?;
        }
        *count = count
            .checked_add(1)
            .ok_or(CacheError::InvalidMapping("open reference count overflow"))?;
        Ok(())
    }

    pub fn close(&mut self, node: NodeId) -> Result<()> {
        let count = self
            .open_nodes
            .get_mut(&node)
            .ok_or(CacheError::NodeNotOpen(node))?;
        *count = count
            .checked_sub(1)
            .ok_or(CacheError::InvalidMapping("node is not open"))?;
        if *count == 0 {
            self.store.on_close_node(node)?;
        }
        Ok(())
    }

    pub fn map_page(
        &mut self,
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
        if self.open_nodes.get(&node).copied().unwrap_or(0) == 0 {
            return Err(CacheError::NodeNotOpen(node));
        }

        let key = (node, offset);
        if self.pages.entry(&key).is_none() {
            let mut bytes = [0; PAGE_SIZE as usize];
            let read = self.store.read_node(node, offset, &mut bytes)?;
            bytes[read..].fill(0);
            self.pages.writes.insert(
                key,
                PageEntry {
                    page: CachePage::from_bytes(&bytes)?,
                    dirty: false,
                },
            );
        }

        if flags.contains(MapFlags::WRITE) && !self.pages.writes.contains_key(&key) {
            let shared = self.pages.entry(&key).expect("page loaded above");
            if self.active_pages.get(&shared.page.id).copied().unwrap_or(0) > 0 {
                return Err(CacheError::PageBusy);
            }
            self.pages.writes.insert(
                key,
                PageEntry {
                    page: shared.page.duplicate()?,
                    dirty: true,
                },
            );
        }

        let entry = self.pages.entry(&key).expect("page loaded above");
        let page = entry.page.clone();
        *self.active_pages.entry(page.id).or_default() += 1;
        Ok(DaxPageMapping {
            cache_id: self.id,
            key,
            flags,
            page,
        })
    }

    pub fn unmap(&mut self, mapping: DaxPageMapping) -> Result<()> {
        if mapping.cache_id != self.id {
            return Err(CacheError::StaleMapping);
        }
        let count = self
            .active_pages
            .get_mut(&mapping.page.id)
            .ok_or(CacheError::StaleMapping)?;
        *count = count.checked_sub(1).ok_or(CacheError::StaleMapping)?;
        if *count == 0 {
            self.active_pages.remove(&mapping.page.id);
        }
        if mapping.flags.contains(MapFlags::WRITE) {
            let entry = self
                .pages
                .writes
                .get_mut(&mapping.key)
                .ok_or(CacheError::StaleMapping)?;
            if entry.page.id != mapping.page.id {
                return Err(CacheError::StaleMapping);
            }
            entry.dirty = true;
        }
        Ok(())
    }

    pub fn read(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file_len = self.store.len(node)?;
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
            let mapping = self.map_page(node, page_offset, MapFlags::READ)?;
            mapping.backing().read_exact_at(
                &mut buf[target_offset..target_offset + chunk_len],
                within_page,
            )?;
            self.unmap(mapping)?;
            cursor += chunk_len as u64;
        }
        Ok(count)
    }

    pub fn write(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> Result<usize> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(CacheError::InvalidMapping("write range overflow"))?;
        let file_len = self.store.len(node)?;
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
            let mapping = self.map_page(node, page_offset, MapFlags::READ | MapFlags::WRITE)?;
            mapping
                .backing()
                .write_all_at(&buf[source_offset..source_offset + chunk_len], within_page)?;
            self.unmap(mapping)?;
            cursor += chunk_len as u64;
        }
        Ok(buf.len())
    }

    pub fn sync(&mut self, node: NodeId) -> Result<()> {
        let dirty_pages: Vec<_> = self
            .pages
            .visible_entries()
            .into_iter()
            .filter(|((entry_node, _), entry)| *entry_node == node && entry.dirty)
            .collect();
        let file_len = self.store.len(node)?;
        for (key, entry) in dirty_pages {
            if key.1 >= file_len {
                continue;
            }
            let count = usize::try_from((file_len - key.1).min(PAGE_SIZE))
                .map_err(|_| CacheError::InvalidMapping("page length does not fit usize"))?;
            let mut bytes = [0; PAGE_SIZE as usize];
            read_exact_at(&entry.page.backing, &mut bytes[..count], 0)?;
            let written = self.store.write_node(node, key.1, &bytes[..count])?;
            if written != count {
                return Err(
                    io::Error::new(io::ErrorKind::WriteZero, "short filesystem write").into(),
                );
            }
            self.pages.writes.insert(
                key,
                PageEntry {
                    page: entry.page,
                    dirty: false,
                },
            );
        }
        Ok(())
    }

    pub fn page_accounting<'a>(caches: impl IntoIterator<Item = &'a Self>) -> CachePageAccounting
    where
        S: 'a,
    {
        let mut references = BTreeMap::<u64, usize>::new();
        for cache in caches {
            let page_ids = cache
                .pages
                .visible_entries()
                .into_values()
                .map(|entry| entry.page.id)
                .collect::<BTreeSet<_>>();
            for page_id in page_ids {
                *references.entry(page_id).or_default() += 1;
            }
        }
        CachePageAccounting {
            resident_pages: references.len(),
            shared_pages: references.values().filter(|&&count| count > 1).count(),
        }
    }
}

impl<S: ForkableNodeStore> BranchingPageCache<S> {
    pub fn fork(&mut self) -> Result<Self> {
        if !self.active_pages.is_empty() {
            return Err(CacheError::ActiveMappings);
        }
        let child_store = self.store.fork_store()?;
        let child_pages = self.pages.fork();
        Ok(Self {
            id: NEXT_CACHE_ID.fetch_add(1, Ordering::Relaxed),
            store: child_store,
            pages: child_pages,
            open_nodes: BTreeMap::new(),
            active_pages: BTreeMap::new(),
        })
    }
}
