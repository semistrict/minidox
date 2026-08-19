//! Host extraction of RedoxFS's `Fmap`/`FileMmapInfo` page cache.
//!
//! RedoxFS maps one anonymous region per inode into many process address spaces.
//! This module preserves that model, but uses one mmap-capable host file per
//! inode so the same pages can be installed into multiple virtiofs DAX windows.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

mod branching;

pub use branching::{BranchingPageCache, CachePageAccounting, DaxPageMapping, ForkableNodeStore};

pub const PAGE_SIZE: u64 = 4096;
pub type NodeId = u32;

bitflags::bitflags! {
    /// Access accumulated for a cached file page.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct MapFlags: u8 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
    }
}

/// The filesystem operations hidden behind the shared page cache.
pub trait NodeStore {
    fn len(&mut self, node: NodeId) -> io::Result<u64>;
    fn read_node(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn write_node(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> io::Result<usize>;

    fn on_open_node(&mut self, _node: NodeId) -> io::Result<()> {
        Ok(())
    }

    fn on_close_node(&mut self, _node: NodeId) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    InvalidMapping(&'static str),
    NodeNotOpen(NodeId),
    StaleMapping,
    ActiveMappings,
    PageBusy,
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => err.fmt(f),
            Self::InvalidMapping(message) => f.write_str(message),
            Self::NodeNotOpen(node) => write!(f, "node {node} is not open"),
            Self::StaleMapping => f.write_str("mapping no longer belongs to this cache"),
            Self::ActiveMappings => f.write_str("cache still has active DAX mappings"),
            Self::PageBusy => f.write_str("DAX page must be unmapped before copy-on-write"),
        }
    }
}

impl std::error::Error for CacheError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, CacheError>;

/// A mapping source suitable for a virtiofs DAX `SETUPMAPPING` operation.
///
/// Every mapping of the same inode references the same file and file offsets.
/// Dropping this value does not unmap it; pass it to [`PageCache::unmap`] so
/// writable data is persisted and the RedoxFS mapping refcount is released.
#[derive(Debug)]
pub struct DaxMapping {
    cache_id: u64,
    node: NodeId,
    offset: u64,
    len: u64,
    flags: MapFlags,
    backing: Arc<File>,
}

impl DaxMapping {
    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn file_offset(&self) -> u64 {
        self.offset
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn flags(&self) -> MapFlags {
        self.flags
    }

    pub fn backing(&self) -> &File {
        &self.backing
    }

    pub fn try_clone_backing(&self) -> io::Result<File> {
        self.backing.try_clone()
    }
}

/// RedoxFS's per-range `Fmap`, represented at DAX page granularity.
#[derive(Clone, Copy, Debug)]
struct Fmap {
    rc: usize,
    flags: MapFlags,
    version: u64,
}

/// RedoxFS's per-inode `FileMmapInfo` with a host-mappable backing file.
#[derive(Debug)]
struct FileMmapInfo {
    backing: Arc<File>,
    size: u64,
    pages: BTreeMap<u64, Fmap>,
    open_fds: usize,
    version: u64,
}

impl FileMmapInfo {
    fn new() -> io::Result<Self> {
        Ok(Self {
            backing: Arc::new(tempfile::tempfile()?),
            size: 0,
            pages: BTreeMap::new(),
            open_fds: 0,
            version: 0,
        })
    }

    fn in_use(&self) -> bool {
        self.open_fds > 0 || self.pages.values().any(|fmap| fmap.rc > 0)
    }

    fn ensure_size(&mut self, size: u64) -> io::Result<()> {
        if size > self.size {
            self.backing.set_len(size)?;
            self.size = size;
        }
        Ok(())
    }
}

/// One shared logical-file page cache for all handles and VM sessions.
///
/// The interface intentionally follows RedoxFS's existing mmap lifecycle:
/// open, map, sync/unmap, close. The virtiofs transport only needs to install
/// [`DaxMapping::backing`] at [`DaxMapping::file_offset`] in a guest DAX window.
pub struct PageCache<S> {
    id: u64,
    store: S,
    files: BTreeMap<NodeId, FileMmapInfo>,
}

impl<S: NodeStore> PageCache<S> {
    pub fn new(store: S) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            store,
            files: BTreeMap::new(),
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn into_store(self) -> S {
        self.store
    }

    pub fn open(&mut self, node: NodeId) -> Result<()> {
        let info = match self.files.entry(node) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(FileMmapInfo::new()?),
        };
        if !info.in_use() {
            self.store.on_open_node(node)?;
        }
        info.open_fds = info
            .open_fds
            .checked_add(1)
            .ok_or(CacheError::InvalidMapping("open reference count overflow"))?;
        Ok(())
    }

    pub fn close(&mut self, node: NodeId) -> Result<()> {
        let info = self
            .files
            .get_mut(&node)
            .ok_or(CacheError::NodeNotOpen(node))?;
        info.open_fds = info
            .open_fds
            .checked_sub(1)
            .ok_or(CacheError::InvalidMapping("node is not open"))?;
        if !info.in_use() {
            self.store.on_close_node(node)?;
        }
        Ok(())
    }

    pub fn map(
        &mut self,
        node: NodeId,
        offset: u64,
        len: u64,
        flags: MapFlags,
    ) -> Result<DaxMapping> {
        if len == 0 {
            return Err(CacheError::InvalidMapping(
                "mapping length must be non-zero",
            ));
        }
        if offset % PAGE_SIZE != 0 {
            return Err(CacheError::InvalidMapping(
                "mapping offset must be page aligned",
            ));
        }
        if !flags.contains(MapFlags::READ) && !flags.contains(MapFlags::WRITE) {
            return Err(CacheError::InvalidMapping(
                "mapping must be readable or writable",
            ));
        }

        let aligned_len = len
            .checked_next_multiple_of(PAGE_SIZE)
            .ok_or(CacheError::InvalidMapping("mapping range overflow"))?;
        let end = offset
            .checked_add(aligned_len)
            .ok_or(CacheError::InvalidMapping("mapping range overflow"))?;
        let info = self
            .files
            .get_mut(&node)
            .ok_or(CacheError::NodeNotOpen(node))?;
        info.ensure_size(end)?;

        for page_offset in (offset..end).step_by(PAGE_SIZE as usize) {
            let must_load = info
                .pages
                .get(&page_offset)
                .is_none_or(|fmap| fmap.version != info.version);
            if must_load {
                load_page(&mut self.store, node, page_offset, &info.backing)?;
            }

            info.pages
                .entry(page_offset)
                .and_modify(|fmap| {
                    fmap.rc += 1;
                    fmap.flags |= flags;
                    fmap.version = info.version;
                })
                .or_insert(Fmap {
                    rc: 1,
                    flags,
                    version: info.version,
                });
        }

        Ok(DaxMapping {
            cache_id: self.id,
            node,
            offset,
            len: aligned_len,
            flags,
            backing: Arc::clone(&info.backing),
        })
    }

    pub fn read(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file_len = self.store.len(node)?;
        if offset >= file_len || buf.is_empty() {
            return Ok(0);
        }
        let count = usize::try_from((file_len - offset).min(buf.len() as u64))
            .map_err(|_| CacheError::InvalidMapping("read length does not fit usize"))?;

        if let Some(info) = self.files.get(&node) {
            let start_page = offset / PAGE_SIZE * PAGE_SIZE;
            let end = (offset + count as u64)
                .checked_next_multiple_of(PAGE_SIZE)
                .ok_or(CacheError::InvalidMapping("read range overflow"))?;
            let fully_cached = (start_page..end).step_by(PAGE_SIZE as usize).all(|page| {
                info.pages
                    .get(&page)
                    .is_some_and(|fmap| fmap.version == info.version)
            });
            if fully_cached {
                read_exact_at(&info.backing, &mut buf[..count], offset)?;
                return Ok(count);
            }
        }

        self.store
            .read_node(node, offset, &mut buf[..count])
            .map_err(Into::into)
    }

    pub fn write(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> Result<usize> {
        let written = self.store.write_node(node, offset, buf)?;
        if let Some(info) = self.files.get_mut(&node) {
            info.version = info.version.wrapping_add(1);
            update_cached_pages(info, offset, &buf[..written])?;
        }
        Ok(written)
    }

    pub fn sync(&mut self, node: NodeId) -> Result<()> {
        let info = self
            .files
            .get_mut(&node)
            .ok_or(CacheError::NodeNotOpen(node))?;
        let pages: Vec<u64> = info
            .pages
            .iter()
            .filter_map(|(offset, fmap)| fmap.flags.contains(MapFlags::WRITE).then_some(*offset))
            .collect();
        for page_offset in pages {
            sync_page(&mut self.store, node, page_offset, info)?;
        }
        Ok(())
    }

    pub fn unmap(&mut self, mapping: DaxMapping) -> Result<()> {
        if mapping.cache_id != self.id {
            return Err(CacheError::StaleMapping);
        }
        let info = self
            .files
            .get_mut(&mapping.node)
            .ok_or(CacheError::NodeNotOpen(mapping.node))?;
        let end = mapping
            .offset
            .checked_add(mapping.len)
            .ok_or(CacheError::InvalidMapping("mapping range overflow"))?;

        for page_offset in (mapping.offset..end).step_by(PAGE_SIZE as usize) {
            let fmap = info
                .pages
                .get(&page_offset)
                .copied()
                .ok_or(CacheError::StaleMapping)?;
            if fmap.flags.contains(MapFlags::WRITE) {
                sync_page(&mut self.store, mapping.node, page_offset, info)?;
            }
            info.pages
                .get_mut(&page_offset)
                .expect("page checked above")
                .rc = fmap.rc.checked_sub(1).ok_or(CacheError::StaleMapping)?;
        }

        if !info.in_use() {
            self.store.on_close_node(mapping.node)?;
        }
        Ok(())
    }
}

fn load_page<S: NodeStore>(store: &mut S, node: NodeId, offset: u64, backing: &File) -> Result<()> {
    let mut page = [0_u8; PAGE_SIZE as usize];
    let count = store.read_node(node, offset, &mut page)?;
    page[count..].fill(0);
    write_all_at(backing, &page, offset)?;
    Ok(())
}

fn sync_page<S: NodeStore>(
    store: &mut S,
    node: NodeId,
    offset: u64,
    info: &mut FileMmapInfo,
) -> Result<()> {
    let file_len = store.len(node)?;
    if offset >= file_len {
        return Ok(());
    }
    let count = usize::try_from((file_len - offset).min(PAGE_SIZE))
        .map_err(|_| CacheError::InvalidMapping("page length does not fit usize"))?;
    let mut page = [0_u8; PAGE_SIZE as usize];
    read_exact_at(&info.backing, &mut page[..count], offset)?;
    let written = store.write_node(node, offset, &page[..count])?;
    if written != count {
        return Err(io::Error::new(io::ErrorKind::WriteZero, "short filesystem write").into());
    }
    if let Some(fmap) = info.pages.get_mut(&offset) {
        fmap.version = info.version;
    }
    Ok(())
}

fn update_cached_pages(info: &mut FileMmapInfo, offset: u64, buf: &[u8]) -> Result<()> {
    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or(CacheError::InvalidMapping("write range overflow"))?;
    for (page_offset, fmap) in info.pages.iter_mut() {
        let page_end = *page_offset + PAGE_SIZE;
        let overlap_start = offset.max(*page_offset);
        let overlap_end = end.min(page_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let source_start = usize::try_from(overlap_start - offset)
            .map_err(|_| CacheError::InvalidMapping("source offset does not fit usize"))?;
        let source_end = source_start + usize::try_from(overlap_end - overlap_start).unwrap();
        write_all_at(&info.backing, &buf[source_start..source_end], overlap_start)?;
        fmap.version = info.version;
    }
    Ok(())
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = file.read_at(buf, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short DAX read",
            ));
        }
        offset += count as u64;
        buf = &mut buf[count..];
    }
    Ok(())
}

fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = file.write_at(buf, offset)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short DAX write"));
        }
        offset += count as u64;
        buf = &buf[count..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct MemoryStore {
        nodes: BTreeMap<NodeId, Vec<u8>>,
        opens: usize,
        closes: usize,
    }

    impl MemoryStore {
        fn with_node(node: NodeId, bytes: &[u8]) -> Self {
            Self {
                nodes: BTreeMap::from([(node, bytes.to_vec())]),
                ..Self::default()
            }
        }
    }

    impl NodeStore for MemoryStore {
        fn len(&mut self, node: NodeId) -> io::Result<u64> {
            Ok(self.nodes.get(&node).map_or(0, |data| data.len()) as u64)
        }

        fn read_node(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
            let data = self.nodes.get(&node).map(Vec::as_slice).unwrap_or_default();
            let start = usize::try_from(offset).unwrap();
            if start >= data.len() {
                return Ok(0);
            }
            let count = buf.len().min(data.len() - start);
            buf[..count].copy_from_slice(&data[start..start + count]);
            Ok(count)
        }

        fn write_node(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> io::Result<usize> {
            let start = usize::try_from(offset).unwrap();
            let data = self.nodes.entry(node).or_default();
            data.resize(data.len().max(start + buf.len()), 0);
            data[start..start + buf.len()].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn on_open_node(&mut self, _node: NodeId) -> io::Result<()> {
            self.opens += 1;
            Ok(())
        }

        fn on_close_node(&mut self, _node: NodeId) -> io::Result<()> {
            self.closes += 1;
            Ok(())
        }
    }

    #[test]
    fn mappings_of_one_inode_share_the_same_pages() {
        let mut cache = PageCache::new(MemoryStore::with_node(7, &[0; PAGE_SIZE as usize]));
        cache.open(7).unwrap();
        cache.open(7).unwrap();
        let first = cache
            .map(7, 0, PAGE_SIZE, MapFlags::READ | MapFlags::WRITE)
            .unwrap();
        let second = cache.map(7, 0, PAGE_SIZE, MapFlags::READ).unwrap();

        assert!(Arc::ptr_eq(&first.backing, &second.backing));
        first.backing().write_at(b"shared", 11).unwrap();
        let mut observed = [0; 6];
        second.backing().read_at(&mut observed, 11).unwrap();
        assert_eq!(&observed, b"shared");

        cache.unmap(second).unwrap();
        cache.unmap(first).unwrap();
        cache.close(7).unwrap();
        cache.close(7).unwrap();
        assert_eq!(&cache.store().nodes[&7][11..17], b"shared");
        assert_eq!(cache.store().opens, 1);
        assert_eq!(cache.store().closes, 1);
    }

    #[test]
    fn normal_write_updates_an_existing_dax_mapping() {
        let mut cache = PageCache::new(MemoryStore::with_node(3, &[0; PAGE_SIZE as usize]));
        cache.open(3).unwrap();
        let mapping = cache.map(3, 0, PAGE_SIZE, MapFlags::READ).unwrap();

        cache.write(3, 100, b"coherent").unwrap();
        let mut observed = [0; 8];
        mapping.backing().read_at(&mut observed, 100).unwrap();
        assert_eq!(&observed, b"coherent");

        cache.unmap(mapping).unwrap();
        cache.close(3).unwrap();
    }

    #[test]
    fn mapping_requires_an_open_node_and_aligned_offset() {
        let mut cache = PageCache::new(MemoryStore::with_node(1, b"data"));
        assert!(matches!(
            cache.map(1, 0, PAGE_SIZE, MapFlags::READ),
            Err(CacheError::NodeNotOpen(1))
        ));
        cache.open(1).unwrap();
        assert!(matches!(
            cache.map(1, 1, PAGE_SIZE, MapFlags::READ),
            Err(CacheError::InvalidMapping(_))
        ));
    }
}
