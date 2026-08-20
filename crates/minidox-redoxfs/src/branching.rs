use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redoxfs::{BLOCK_SIZE, Disk, FileSystem, Node, TreePtr};
use serde::{Deserialize, Serialize};
use syscall::error::{EIO, Error, Result as SyscallResult};

use minidox_cache::{ForkableNodeStore, NodeId, NodeStore, PAGE_SIZE};

static NEXT_BLOCK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FILE_OBJECT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PAGE_VERSION: AtomicU64 = AtomicU64::new(1);
static NEXT_DURABLE_LAYER_ID: AtomicU64 = AtomicU64::new(1);

const LAYER_RECORD_SIZE: usize = 16 + BLOCK_SIZE as usize;

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

    fn visible_files(&self, files: &mut BTreeMap<NodeId, u64>) {
        if let Some(parent) = &self.parent {
            parent.visible_files(files);
        }
        files.extend(self.files.iter().map(|(&node, &object)| (node, object)));
    }

    fn visible_pages(&self, pages: &mut BTreeMap<PageKey, u64>) {
        if let Some(parent) = &self.parent {
            parent.visible_pages(pages);
        }
        pages.extend(self.pages.iter().map(|(&key, &version)| (key, version)));
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

    fn visible_files(&self) -> BTreeMap<NodeId, u64> {
        let mut files = BTreeMap::new();
        self.base.visible_files(&mut files);
        files.extend(self.files.iter().map(|(&node, &object)| (node, object)));
        files
    }

    fn visible_pages(&self) -> BTreeMap<PageKey, u64> {
        let mut pages = BTreeMap::new();
        self.base.visible_pages(&mut pages);
        pages.extend(self.pages.iter().map(|(&key, &version)| (key, version)));
        pages
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DurableDiskState {
    pub size: u64,
    pub layers: Vec<u64>,
    pub overlay: u64,
}

#[derive(Debug)]
struct DurableCowDisk {
    directory: Arc<PathBuf>,
    layers: Vec<u64>,
    overlay_id: u64,
    overlay: File,
}

impl DurableCowDisk {
    fn create(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = Arc::new(directory.as_ref().to_path_buf());
        fs::create_dir_all(directory.as_ref())?;
        let (overlay_id, overlay) = create_numbered_file(&directory, "overlay")?;
        sync_directory(&directory)?;
        Ok(Self {
            directory,
            layers: Vec::new(),
            overlay_id,
            overlay,
        })
    }

    fn restore(
        directory: impl AsRef<Path>,
        state: DurableDiskState,
        objects: &mut BTreeMap<u64, Arc<Block>>,
    ) -> io::Result<(Self, Arc<DiskGeneration>, BTreeMap<u64, Arc<Block>>)> {
        let directory = Arc::new(directory.as_ref().to_path_buf());
        let mut base_blocks = BTreeMap::new();
        for &layer in &state.layers {
            load_records(
                &directory.join(format!("layer-{layer}")),
                objects,
                &mut base_blocks,
            )?;
        }
        let overlay_path = directory.join(format!("overlay-{}", state.overlay));
        let mut snapshot_writes = BTreeMap::new();
        load_records(&overlay_path, objects, &mut snapshot_writes)?;
        NEXT_DURABLE_LAYER_ID.fetch_max(
            state
                .layers
                .iter()
                .copied()
                .chain(std::iter::once(state.overlay))
                .max()
                .unwrap_or(0)
                .saturating_add(1),
            Ordering::Relaxed,
        );
        let mut live_layers = state.layers;
        if overlay_path.metadata()?.len() > 0 {
            let (layer, layer_path) = link_numbered_file(&directory, "layer", &overlay_path)?;
            File::open(layer_path)?.sync_data()?;
            live_layers.push(layer);
        }
        let (overlay_id, overlay) = create_numbered_file(&directory, "overlay")?;
        sync_directory(&directory)?;
        base_blocks.extend(snapshot_writes);
        Ok((
            Self {
                directory,
                layers: live_layers,
                overlay_id,
                overlay,
            },
            Arc::new(DiskGeneration {
                parent: None,
                blocks: base_blocks,
            }),
            BTreeMap::new(),
        ))
    }

    fn append(&mut self, address: u64, block: &Block) -> io::Result<()> {
        self.overlay.write_all(&address.to_le_bytes())?;
        self.overlay.write_all(&block.id.to_le_bytes())?;
        self.overlay.write_all(&block.bytes)?;
        Ok(())
    }

    fn fork(&mut self, seal_overlay: bool) -> io::Result<Self> {
        let child_layers = if seal_overlay {
            let (layer_id, _) = link_numbered_file(
                &self.directory,
                "layer",
                self.directory.join(format!("overlay-{}", self.overlay_id)),
            )?;
            self.layers.push(layer_id);
            self.layers.clone()
        } else {
            self.layers.clone()
        };

        if seal_overlay {
            let (overlay_id, overlay) = create_numbered_file(&self.directory, "overlay")?;
            self.overlay_id = overlay_id;
            self.overlay = overlay;
        }
        let (child_overlay_id, child_overlay) = create_numbered_file(&self.directory, "overlay")?;
        Ok(Self {
            directory: Arc::clone(&self.directory),
            layers: child_layers,
            overlay_id: child_overlay_id,
            overlay: child_overlay,
        })
    }

    fn checkpoint(&mut self, size: u64, seal_overlay: bool) -> io::Result<DurableDiskState> {
        for layer in &self.layers {
            File::open(self.directory.join(format!("layer-{layer}")))?.sync_data()?;
        }
        self.overlay.sync_data()?;
        let state = DurableDiskState {
            size,
            layers: self.layers.clone(),
            overlay: self.overlay_id,
        };

        let layer = if seal_overlay {
            let (layer_id, layer_path) = link_numbered_file(
                &self.directory,
                "layer",
                self.directory.join(format!("overlay-{}", self.overlay_id)),
            )?;
            File::open(layer_path)?.sync_data()?;
            Some(layer_id)
        } else {
            None
        };
        let (next_overlay_id, next_overlay) = create_numbered_file(&self.directory, "overlay")?;
        sync_directory(&self.directory)?;
        if let Some(layer) = layer {
            self.layers.push(layer);
        }
        self.overlay_id = next_overlay_id;
        self.overlay = next_overlay;
        Ok(state)
    }
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

fn next_durable_id() -> u64 {
    NEXT_DURABLE_LAYER_ID.fetch_add(1, Ordering::Relaxed)
}

fn create_numbered_file(directory: &Path, prefix: &str) -> io::Result<(u64, File)> {
    loop {
        let id = next_durable_id();
        let path = directory.join(format!("{prefix}-{id}"));
        match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => return Ok((id, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn link_numbered_file(
    directory: &Path,
    prefix: &str,
    source: impl AsRef<Path>,
) -> io::Result<(u64, PathBuf)> {
    loop {
        let id = next_durable_id();
        let path = directory.join(format!("{prefix}-{id}"));
        match fs::hard_link(source.as_ref(), &path) {
            Ok(()) => return Ok((id, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn load_records(
    path: &Path,
    objects: &mut BTreeMap<u64, Arc<Block>>,
    visible: &mut BTreeMap<u64, Arc<Block>>,
) -> io::Result<()> {
    let bytes = fs::read(path)?;
    if bytes.len() % LAYER_RECORD_SIZE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "filesystem layer {} ends with a partial block record",
                path.display()
            ),
        ));
    }
    for record in bytes.chunks_exact(LAYER_RECORD_SIZE) {
        let address = u64::from_le_bytes(record[..8].try_into().unwrap());
        let id = u64::from_le_bytes(record[8..16].try_into().unwrap());
        let block_bytes: [u8; BLOCK_SIZE as usize] = record[16..].try_into().unwrap();
        let block = if let Some(existing) = objects.get(&id) {
            if existing.bytes != block_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("filesystem block object {id} has conflicting contents"),
                ));
            }
            Arc::clone(existing)
        } else {
            let block = Arc::new(Block {
                id,
                bytes: block_bytes,
            });
            objects.insert(id, Arc::clone(&block));
            block
        };
        NEXT_BLOCK_ID.fetch_max(id.saturating_add(1), Ordering::Relaxed);
        visible.insert(address, block);
    }
    Ok(())
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

#[derive(Debug)]
struct CowDisk {
    size: u64,
    base: Arc<DiskGeneration>,
    writes: BTreeMap<u64, Arc<Block>>,
    durable: Option<DurableCowDisk>,
}

impl CowDisk {
    fn new(size: u64) -> Self {
        Self {
            size,
            base: Arc::new(DiskGeneration::default()),
            writes: BTreeMap::new(),
            durable: None,
        }
    }

    fn durable(directory: impl AsRef<Path>, size: u64) -> io::Result<Self> {
        Ok(Self {
            size,
            base: Arc::new(DiskGeneration::default()),
            writes: BTreeMap::new(),
            durable: Some(DurableCowDisk::create(directory)?),
        })
    }

    fn fork(&mut self) -> io::Result<Self> {
        let has_writes = !self.writes.is_empty();
        let child_durable = self
            .durable
            .as_mut()
            .map(|durable| durable.fork(has_writes))
            .transpose()?;
        if has_writes {
            self.base = Arc::new(DiskGeneration {
                parent: Some(self.base.clone()),
                blocks: std::mem::take(&mut self.writes),
            });
        }

        Ok(Self {
            size: self.size,
            base: self.base.clone(),
            writes: BTreeMap::new(),
            durable: child_durable,
        })
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

    fn durable_state(&mut self) -> io::Result<DurableDiskState> {
        let size = self.size;
        let has_writes = !self.writes.is_empty();
        let state = self
            .durable
            .as_mut()
            .ok_or_else(|| io::Error::other("filesystem branch is not durable"))?
            .checkpoint(size, has_writes)?;
        if has_writes {
            self.base = Arc::new(DiskGeneration {
                parent: Some(Arc::clone(&self.base)),
                blocks: std::mem::take(&mut self.writes),
            });
        }
        Ok(state)
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
            if let Some(durable) = &mut self.durable {
                durable
                    .append(address, &self.writes[&address])
                    .map_err(|_| Error::new(EIO))?;
            }
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedoxFileIdentity {
    pub node: NodeId,
    pub object: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedoxPageVersion {
    pub node: NodeId,
    pub page: u64,
    pub version: u64,
}

/// Durable branch metadata. RedoxFS block data remains in native Disk layers.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RedoxBranchState {
    pub disk: DurableDiskState,
    pub header_block: u64,
    pub files: Vec<RedoxFileIdentity>,
    pub pages: Vec<RedoxPageVersion>,
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

    pub fn create_durable(directory: impl AsRef<Path>, size: u64) -> io::Result<Self> {
        let fs =
            FileSystem::create(CowDisk::durable(directory, size)?, None, 1, 0).map_err(io_error)?;
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
        let child_disk = self.fs.disk.fork()?;
        let child_identities = self.identities.fork();
        let child =
            FileSystem::open(child_disk, None, Some(self.fs.block), false).map_err(io_error)?;
        Ok(Self {
            fs: child,
            identities: child_identities,
        })
    }

    pub fn durable_state(&mut self) -> io::Result<RedoxBranchState> {
        let disk = self.fs.disk.durable_state()?;
        let files = self
            .identities
            .visible_files()
            .into_iter()
            .map(|(node, object)| RedoxFileIdentity { node, object })
            .collect();
        let pages = self
            .identities
            .visible_pages()
            .into_iter()
            .map(|((node, page), version)| RedoxPageVersion {
                node,
                page,
                version,
            })
            .collect();
        Ok(RedoxBranchState {
            disk,
            header_block: self.fs.block,
            files,
            pages,
        })
    }

    pub fn restore_lineage(
        directory: impl AsRef<Path>,
        states: Vec<RedoxBranchState>,
    ) -> io::Result<Vec<Self>> {
        let mut objects = BTreeMap::new();
        let mut max_file = 0;
        let mut max_version = 0;
        let mut branches = Vec::with_capacity(states.len());
        for state in states {
            let (durable, base, writes) =
                DurableCowDisk::restore(directory.as_ref(), state.disk.clone(), &mut objects)?;
            let disk = CowDisk {
                size: state.disk.size,
                base,
                writes,
                durable: Some(durable),
            };
            let fs =
                FileSystem::open(disk, None, Some(state.header_block), false).map_err(io_error)?;
            let files = state
                .files
                .into_iter()
                .map(|identity| {
                    max_file = max_file.max(identity.object);
                    (identity.node, identity.object)
                })
                .collect();
            let pages = state
                .pages
                .into_iter()
                .map(|page| {
                    max_version = max_version.max(page.version);
                    ((page.node, page.page), page.version)
                })
                .collect();
            branches.push(Self {
                fs,
                identities: IdentityBranch {
                    base: Arc::new(IdentityGeneration::default()),
                    files,
                    pages,
                },
            });
        }
        NEXT_FILE_OBJECT_ID.fetch_max(max_file.saturating_add(1), Ordering::Relaxed);
        NEXT_PAGE_VERSION.fetch_max(max_version.saturating_add(1), Ordering::Relaxed);
        Ok(branches)
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
