use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redoxfs::{BLOCK_SIZE, Disk, FileSystem, Node, TreePtr};
use syscall::error::{EIO, Error, Result as SyscallResult};

use minidox_cache::{ForkableNodeStore, NodeId, NodeStore, PAGE_SIZE};

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FILE_OBJECT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PAGE_VERSION: AtomicU64 = AtomicU64::new(1);

type PageKey = (NodeId, u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RedoxPageIdentity(u64, u64, u64);

#[derive(Debug, Default)]
struct IdentityGeneration {
    parent: Option<Arc<IdentityGeneration>>,
    files: BTreeMap<NodeId, u64>,
    pages: BTreeMap<PageKey, u64>,
}

impl IdentityGeneration {
    fn file(&self, node: NodeId) -> Option<u64> {
        self.files
            .get(&node)
            .copied()
            .or_else(|| self.parent.as_deref().and_then(|parent| parent.file(node)))
    }

    fn page_version(&self, key: &PageKey) -> Option<u64> {
        self.pages.get(key).copied().or_else(|| {
            self.parent
                .as_deref()
                .and_then(|parent| parent.page_version(key))
        })
    }
}

#[derive(Debug)]
struct IdentityBranch {
    base: Arc<IdentityGeneration>,
    files: BTreeMap<NodeId, u64>,
    pages: BTreeMap<PageKey, u64>,
}

impl IdentityBranch {
    fn new() -> Self {
        Self {
            base: Arc::new(IdentityGeneration::default()),
            files: BTreeMap::new(),
            pages: BTreeMap::new(),
        }
    }

    fn fork(&mut self) -> Self {
        if !self.files.is_empty() || !self.pages.is_empty() {
            self.base = Arc::new(IdentityGeneration {
                parent: Some(Arc::clone(&self.base)),
                files: std::mem::take(&mut self.files),
                pages: std::mem::take(&mut self.pages),
            });
        }
        Self {
            base: Arc::clone(&self.base),
            files: BTreeMap::new(),
            pages: BTreeMap::new(),
        }
    }

    fn create_file(&mut self, node: NodeId) {
        self.files
            .insert(node, NEXT_FILE_OBJECT_ID.fetch_add(1, Ordering::Relaxed));
    }

    fn page_identity(&self, node: NodeId, offset: u64) -> io::Result<RedoxPageIdentity> {
        let object = self
            .files
            .get(&node)
            .copied()
            .or_else(|| self.base.file(node))
            .ok_or_else(|| io::Error::from_raw_os_error(EIO))?;
        let page = offset / PAGE_SIZE;
        let version = self
            .pages
            .get(&(node, page))
            .copied()
            .or_else(|| self.base.page_version(&(node, page)))
            .unwrap_or(0);
        Ok(RedoxPageIdentity(object, page, version))
    }

    fn record_write(&mut self, node: NodeId, offset: u64, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(EIO))?;
        let first = offset / PAGE_SIZE;
        let last = (end - 1) / PAGE_SIZE;
        for page in first..=last {
            self.pages.insert(
                (node, page),
                NEXT_PAGE_VERSION.fetch_add(1, Ordering::Relaxed),
            );
        }
        Ok(())
    }

    fn prepare_page_write(&mut self, node: NodeId, offset: u64) -> io::Result<RedoxPageIdentity> {
        let page = offset / PAGE_SIZE;
        self.pages.insert(
            (node, page),
            NEXT_PAGE_VERSION.fetch_add(1, Ordering::Relaxed),
        );
        self.page_identity(node, offset)
    }
}

#[derive(Debug)]
struct Block {
    id: u64,
    bytes: [u8; BLOCK_SIZE as usize],
}

#[derive(Debug, Default)]
struct DiskGeneration {
    parent: Option<Arc<DiskGeneration>>,
    blocks: BTreeMap<u64, Arc<Block>>,
}

impl DiskGeneration {
    fn block(&self, block: u64) -> Option<&Arc<Block>> {
        self.blocks.get(&block).or_else(|| {
            self.parent
                .as_deref()
                .and_then(|parent| parent.block(block))
        })
    }

    fn visible_blocks(&self, blocks: &mut BTreeMap<u64, Arc<Block>>) {
        if let Some(parent) = &self.parent {
            parent.visible_blocks(blocks);
        }
        blocks.extend(
            self.blocks
                .iter()
                .map(|(&address, block)| (address, block.clone())),
        );
    }
}

#[derive(Debug)]
struct CowDisk {
    size: u64,
    base: Arc<DiskGeneration>,
    writes: BTreeMap<u64, Arc<Block>>,
}

impl CowDisk {
    fn new(size: u64) -> Self {
        Self {
            size,
            base: Arc::new(DiskGeneration::default()),
            writes: BTreeMap::new(),
        }
    }

    fn fork(&mut self) -> Self {
        if !self.writes.is_empty() {
            self.base = Arc::new(DiskGeneration {
                parent: Some(self.base.clone()),
                blocks: std::mem::take(&mut self.writes),
            });
        }

        Self {
            size: self.size,
            base: self.base.clone(),
            writes: BTreeMap::new(),
        }
    }

    fn visible_block(&self, block: u64) -> Option<&Arc<Block>> {
        self.writes.get(&block).or_else(|| self.base.block(block))
    }

    fn visible_blocks(&self) -> BTreeMap<u64, Arc<Block>> {
        let mut blocks = BTreeMap::new();
        self.base.visible_blocks(&mut blocks);
        blocks.extend(
            self.writes
                .iter()
                .map(|(&address, block)| (address, block.clone())),
        );
        blocks
    }

    fn check_range(&self, block: u64, len: usize) -> SyscallResult<()> {
        let offset = block
            .checked_mul(BLOCK_SIZE)
            .ok_or_else(|| Error::new(EIO))?;
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| Error::new(EIO))?;
        if end > self.size {
            return Err(Error::new(EIO));
        }
        Ok(())
    }
}

impl Disk for CowDisk {
    unsafe fn read_at(&mut self, block: u64, buffer: &mut [u8]) -> SyscallResult<usize> {
        self.check_range(block, buffer.len())?;

        for (index, chunk) in buffer.chunks_mut(BLOCK_SIZE as usize).enumerate() {
            let address = block + index as u64;
            if let Some(stored) = self.visible_block(address) {
                chunk.copy_from_slice(&stored.bytes[..chunk.len()]);
            } else {
                chunk.fill(0);
            }
        }
        Ok(buffer.len())
    }

    unsafe fn write_at(&mut self, block: u64, buffer: &[u8]) -> SyscallResult<usize> {
        self.check_range(block, buffer.len())?;

        for (index, chunk) in buffer.chunks(BLOCK_SIZE as usize).enumerate() {
            let address = block + index as u64;
            let mut bytes = self
                .visible_block(address)
                .map_or([0; BLOCK_SIZE as usize], |stored| stored.bytes);
            bytes[..chunk.len()].copy_from_slice(chunk);
            self.writes.insert(
                address,
                Arc::new(Block {
                    id: NEXT_BLOCK_ID.fetch_add(1, Ordering::Relaxed),
                    bytes,
                }),
            );
        }
        Ok(buffer.len())
    }

    fn size(&mut self) -> SyscallResult<u64> {
        Ok(self.size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockAccounting {
    pub resident_blocks: usize,
    pub shared_blocks: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeMetadata {
    pub id: NodeId,
    pub size: u64,
    pub blocks_512: u64,
    pub mode: u32,
    pub links: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub id: NodeId,
    pub name: String,
}

/// An independently writable RedoxFS branch over shared immutable disk blocks.
pub struct RedoxBranch {
    fs: FileSystem<CowDisk>,
    identities: IdentityBranch,
}

impl RedoxBranch {
    pub fn create(size: u64) -> io::Result<Self> {
        let fs = FileSystem::create(CowDisk::new(size), None, 1, 0).map_err(io_error)?;
        let mut identities = IdentityBranch::new();
        identities.create_file(TreePtr::<Node>::root().id());
        Ok(Self { fs, identities })
    }

    pub fn create_file(&mut self, name: &str, size: u64) -> io::Result<NodeId> {
        let node = self
            .fs
            .tx(|tx| {
                let node =
                    tx.create_node(TreePtr::<Node>::root(), name, Node::MODE_FILE | 0o644, 1, 0)?;
                tx.truncate_node(node.ptr(), size, 1, 0)?;
                Ok(node.ptr().id())
            })
            .map_err(io_error)?;
        self.identities.create_file(node);
        Ok(node)
    }

    pub fn read(&mut self, node: NodeId, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; len];
        let read = self
            .fs
            .tx(|tx| tx.read_node(TreePtr::<Node>::new(node), offset, &mut bytes, 1, 0))
            .map_err(io_error)?;
        bytes.truncate(read);
        Ok(bytes)
    }

    pub fn write(&mut self, node: NodeId, offset: u64, bytes: &[u8]) -> io::Result<usize> {
        let written = self
            .fs
            .tx(|tx| tx.write_node(TreePtr::<Node>::new(node), offset, bytes, 1, 0))
            .map_err(io_error)?;
        self.identities.record_write(node, offset, written)?;
        Ok(written)
    }

    pub fn metadata(&mut self, node: NodeId) -> io::Result<NodeMetadata> {
        self.fs
            .tx(|tx| {
                let node = tx.read_tree(TreePtr::<Node>::new(node))?;
                let data = node.data();
                Ok(NodeMetadata {
                    id: node.id(),
                    size: data.size(),
                    blocks_512: data.blocks() * (BLOCK_SIZE / 512),
                    mode: data.mode().into(),
                    links: data.links(),
                    uid: data.uid(),
                    gid: data.gid(),
                    atime: data.atime(),
                    mtime: data.mtime(),
                    ctime: data.ctime(),
                })
            })
            .map_err(io_error)
    }

    pub fn lookup(&mut self, parent: NodeId, name: &str) -> io::Result<NodeId> {
        self.fs
            .tx(|tx| {
                tx.find_node(TreePtr::<Node>::new(parent), name)
                    .map(|node| node.ptr().id())
            })
            .map_err(io_error)
    }

    pub fn entries(&mut self, parent: NodeId) -> io::Result<Vec<DirectoryEntry>> {
        self.fs
            .tx(|tx| {
                let mut entries = Vec::new();
                tx.child_nodes(TreePtr::<Node>::new(parent), &mut entries)?;
                entries
                    .into_iter()
                    .map(|entry| {
                        Ok(DirectoryEntry {
                            id: entry.node_ptr().id(),
                            name: entry.name().ok_or_else(|| Error::new(EIO))?.to_owned(),
                        })
                    })
                    .collect()
            })
            .map_err(io_error)
    }

    pub fn fork(&mut self) -> io::Result<Self> {
        let child_disk = self.fs.disk.fork();
        let child_identities = self.identities.fork();
        let child =
            FileSystem::open(child_disk, None, Some(self.fs.block), false).map_err(io_error)?;
        Ok(Self {
            fs: child,
            identities: child_identities,
        })
    }

    pub fn block_accounting<'a>(branches: impl IntoIterator<Item = &'a Self>) -> BlockAccounting {
        let mut references = BTreeMap::<u64, usize>::new();
        for branch in branches {
            let branch_ids = branch
                .fs
                .disk
                .visible_blocks()
                .into_values()
                .map(|block| block.id)
                .collect::<BTreeSet<_>>();
            for id in branch_ids {
                *references.entry(id).or_default() += 1;
            }
        }

        BlockAccounting {
            resident_blocks: references.len(),
            shared_blocks: references.values().filter(|&&count| count > 1).count(),
        }
    }
}

impl NodeStore for RedoxBranch {
    fn len(&mut self, node: NodeId) -> io::Result<u64> {
        self.fs
            .tx(|tx| {
                let node = tx.read_tree(TreePtr::<Node>::new(node))?;
                Ok(node.data().size())
            })
            .map_err(io_error)
    }

    fn read_node(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.fs
            .tx(|tx| tx.read_node(TreePtr::<Node>::new(node), offset, buf, 1, 0))
            .map_err(io_error)
    }

    fn write_node(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> io::Result<usize> {
        self.fs
            .tx(|tx| tx.write_node(TreePtr::<Node>::new(node), offset, buf, 1, 0))
            .map_err(io_error)
    }

    fn on_open_node(&mut self, node: NodeId) -> io::Result<()> {
        self.fs
            .tx(|tx| tx.on_open_node(TreePtr::<Node>::new(node)))
            .map_err(io_error)
    }

    fn on_close_node(&mut self, node: NodeId) -> io::Result<()> {
        self.fs
            .tx(|tx| tx.on_close_node(TreePtr::<Node>::new(node)))
            .map_err(io_error)
    }
}

impl ForkableNodeStore for RedoxBranch {
    type PageIdentity = RedoxPageIdentity;

    fn page_identity(&mut self, node: NodeId, offset: u64) -> io::Result<Self::PageIdentity> {
        self.identities.page_identity(node, offset)
    }

    fn prepare_page_write(&mut self, node: NodeId, offset: u64) -> io::Result<Self::PageIdentity> {
        self.identities.prepare_page_write(node, offset)
    }

    fn fork_store(&mut self) -> io::Result<Self> {
        self.fork()
    }
}

fn io_error(error: Error) -> io::Error {
    io::Error::from_raw_os_error(error.errno)
}
