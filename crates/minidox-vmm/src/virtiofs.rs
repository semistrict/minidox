use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use minidox_cache::{
    BranchingPageCache, CachePageAccounting, DaxPageMapping, MapFlags, NodeId, NodeStore, PAGE_SIZE,
};
use minidox_redoxfs::{NodeMetadata, RedoxBranch};
use virtio_devices::{FsDirEntry, FsNodeAttr, InProcessFsBackend, register_in_process_fs};

use crate::Error;

const FILESYSTEM_SIZE: u64 = 32 * 1024 * 1024;
const DAX_WINDOW_SIZE: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct MappingSpec {
    inode: NodeId,
    file_offset: u64,
    writable: bool,
}

struct State {
    cache: BranchingPageCache<RedoxBranch>,
    window: Option<(usize, usize)>,
    specs: BTreeMap<u64, MappingSpec>,
    active: BTreeMap<u64, DaxPageMapping>,
}

/// One RedoxFS branch and the DAX mappings installed for its guest.
pub(crate) struct VirtioFsBranch {
    state: Mutex<State>,
    backend_id: AtomicU64,
}

impl VirtioFsBranch {
    pub(crate) fn create() -> Result<Arc<Self>, Error> {
        Self::from_parts(
            BranchingPageCache::new(
                RedoxBranch::create(FILESYSTEM_SIZE)
                    .map_err(|error| Error::backend("create RedoxFS branch", error))?,
            ),
            BTreeMap::new(),
        )
    }

    fn from_parts(
        mut cache: BranchingPageCache<RedoxBranch>,
        specs: BTreeMap<u64, MappingSpec>,
    ) -> Result<Arc<Self>, Error> {
        for inode in specs
            .values()
            .map(|spec| spec.inode)
            .collect::<BTreeSet<_>>()
        {
            cache
                .open(inode)
                .map_err(|error| Error::backend("restore DAX file handle", error))?;
        }
        let branch = Arc::new(Self {
            state: Mutex::new(State {
                cache,
                window: None,
                specs,
                active: BTreeMap::new(),
            }),
            backend_id: AtomicU64::new(0),
        });
        let backend: Arc<dyn InProcessFsBackend> = branch.clone();
        branch
            .backend_id
            .store(register_in_process_fs(&backend), Ordering::Release);
        Ok(branch)
    }

    pub(crate) fn backend_id(&self) -> u64 {
        self.backend_id.load(Ordering::Acquire)
    }

    pub(crate) fn dax_window_size(&self) -> u64 {
        DAX_WINDOW_SIZE
    }

    pub(crate) fn fork(&self) -> Result<Arc<Self>, Error> {
        let mut state = self.state.lock().unwrap();
        suspend_mappings(&mut state)
            .map_err(|error| Error::backend("suspend DAX mappings", error))?;
        let child = state
            .cache
            .fork()
            .map(|cache| (cache, state.specs.clone()))
            .map_err(|error| Error::backend("fork RedoxFS/DAX branch", error));
        let resume = resume_mappings(&mut state)
            .map_err(|error| Error::backend("resume DAX mappings", error));
        let (child_cache, child_specs) = child?;
        resume?;
        drop(state);
        Self::from_parts(child_cache, child_specs)
    }

    pub(crate) fn create_file(&self, name: &str, size: u64) -> Result<NodeId, Error> {
        self.state
            .lock()
            .unwrap()
            .cache
            .store_mut()
            .create_file(name, size)
            .map_err(|error| Error::backend("create RedoxFS file", error))
    }

    pub(crate) fn read(&self, node: NodeId, offset: u64, len: usize) -> Result<Vec<u8>, Error> {
        let mut state = self.state.lock().unwrap();
        state
            .cache
            .open(node)
            .map_err(|error| Error::backend("open cached file", error))?;
        let mut bytes = vec![0; len];
        let result = state
            .cache
            .read(node, offset, &mut bytes)
            .map_err(|error| Error::backend("read cached file", error));
        let close = state
            .cache
            .close(node)
            .map_err(|error| Error::backend("close cached file", error));
        let read = result?;
        close?;
        bytes.truncate(read);
        Ok(bytes)
    }

    pub(crate) fn write(&self, node: NodeId, offset: u64, bytes: &[u8]) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();
        suspend_mappings(&mut state)
            .map_err(|error| Error::backend("suspend DAX mappings for write", error))?;
        let result = (|| {
            state
                .cache
                .open(node)
                .map_err(|error| Error::backend("open cached file", error))?;
            let write = state
                .cache
                .write(node, offset, bytes)
                .and_then(|_| state.cache.sync(node))
                .map_err(|error| Error::backend("write cached file", error));
            let close = state
                .cache
                .close(node)
                .map_err(|error| Error::backend("close cached file", error));
            write?;
            close
        })();
        let resume = resume_mappings(&mut state)
            .map_err(|error| Error::backend("resume DAX mappings after write", error));
        result?;
        resume
    }

    pub(crate) fn page_accounting<'a>(
        branches: impl IntoIterator<Item = &'a Arc<Self>>,
    ) -> CachePageAccounting {
        let guards = branches
            .into_iter()
            .map(|branch| branch.state.lock().unwrap())
            .collect::<Vec<_>>();
        BranchingPageCache::page_accounting(guards.iter().map(|state| &state.cache))
    }
}

impl InProcessFsBackend for VirtioFsBranch {
    fn lookup(&self, parent: u64, name: &str) -> io::Result<FsNodeAttr> {
        let mut state = self.state.lock().unwrap();
        let inode = state.cache.store_mut().lookup(node_id(parent)?, name)?;
        state.cache.store_mut().metadata(inode).map(node_attr)
    }

    fn getattr(&self, inode: u64) -> io::Result<FsNodeAttr> {
        self.state
            .lock()
            .unwrap()
            .cache
            .store_mut()
            .metadata(node_id(inode)?)
            .map(node_attr)
    }

    fn open(&self, inode: u64) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .cache
            .open(node_id(inode)?)
            .map_err(io::Error::other)
    }

    fn close(&self, inode: u64) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .cache
            .close(node_id(inode)?)
            .map_err(io::Error::other)
    }

    fn read(&self, inode: u64, offset: u64, data: &mut [u8]) -> io::Result<usize> {
        self.state
            .lock()
            .unwrap()
            .cache
            .read(node_id(inode)?, offset, data)
            .map_err(io::Error::other)
    }

    fn write(&self, inode: u64, offset: u64, data: &[u8]) -> io::Result<usize> {
        self.state
            .lock()
            .unwrap()
            .cache
            .write(node_id(inode)?, offset, data)
            .map_err(io::Error::other)
    }

    fn sync(&self, inode: u64) -> io::Result<()> {
        self.state
            .lock()
            .unwrap()
            .cache
            .sync(node_id(inode)?)
            .map_err(io::Error::other)
    }

    fn entries(&self, inode: u64) -> io::Result<Vec<FsDirEntry>> {
        let mut state = self.state.lock().unwrap();
        state
            .cache
            .store_mut()
            .entries(node_id(inode)?)?
            .into_iter()
            .map(|entry| {
                let metadata = state.cache.store_mut().metadata(entry.id)?;
                Ok(FsDirEntry {
                    inode: entry.id.into(),
                    name: entry.name,
                    directory: metadata.mode & libc::S_IFMT == libc::S_IFDIR,
                })
            })
            .collect()
    }

    fn attach_dax_window(&self, host_address: usize, len: usize) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.window.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "DAX window is already attached",
            ));
        }
        state.window = Some((host_address, len));
        if let Err(error) = resume_mappings(&mut state) {
            state.window = None;
            return Err(error);
        }
        Ok(())
    }

    fn detach_dax_window(&self, host_address: usize, len: usize) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.window != Some((host_address, len)) {
            return Ok(());
        }
        suspend_mappings(&mut state)?;
        state.window = None;
        Ok(())
    }

    fn setup_mapping(
        &self,
        inode: u64,
        file_offset: u64,
        len: u64,
        writable: bool,
        window_offset: u64,
    ) -> io::Result<()> {
        if len == 0
            || file_offset % PAGE_SIZE != 0
            || window_offset % PAGE_SIZE != 0
            || len % PAGE_SIZE != 0
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut state = self.state.lock().unwrap();
        let (_, window_len) = state
            .window
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODEV))?;
        let end = window_offset
            .checked_add(len)
            .filter(|end| *end <= window_len as u64)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        file_offset
            .checked_add(len)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        if state
            .specs
            .range(..end)
            .any(|(&offset, _)| offset >= window_offset || offset + PAGE_SIZE > window_offset)
        {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        let inode = node_id(inode)?;
        let mut inserted = Vec::new();
        for delta in (0..len).step_by(PAGE_SIZE as usize) {
            let offset = window_offset + delta;
            state.specs.insert(
                offset,
                MappingSpec {
                    inode,
                    file_offset: file_offset + delta,
                    writable,
                },
            );
            inserted.push(offset);
        }
        if let Err(error) = activate_offsets(&mut state, &inserted) {
            for offset in inserted {
                state.specs.remove(&offset);
            }
            return Err(error);
        }
        Ok(())
    }

    fn remove_mapping(&self, window_offset: u64, len: u64) -> io::Result<()> {
        if len == 0 || window_offset % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut state = self.state.lock().unwrap();
        for delta in (0..len).step_by(PAGE_SIZE as usize) {
            remove_offset(&mut state, window_offset + delta, true)?;
        }
        Ok(())
    }
}

fn node_id(inode: u64) -> io::Result<NodeId> {
    inode
        .try_into()
        .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
}

fn node_attr(metadata: NodeMetadata) -> FsNodeAttr {
    FsNodeAttr {
        inode: metadata.id.into(),
        size: metadata.size,
        blocks: metadata.blocks_512,
        atime: metadata.atime,
        mtime: metadata.mtime,
        ctime: metadata.ctime,
        mode: metadata.mode,
        links: metadata.links,
        uid: metadata.uid,
        gid: metadata.gid,
    }
}

fn suspend_mappings(state: &mut State) -> io::Result<()> {
    let offsets = state.active.keys().copied().collect::<Vec<_>>();
    for offset in offsets {
        remove_offset(state, offset, false)?;
    }
    Ok(())
}

fn resume_mappings(state: &mut State) -> io::Result<()> {
    let offsets = state.specs.keys().copied().collect::<Vec<_>>();
    activate_offsets(state, &offsets)
}

fn activate_offsets(state: &mut State, offsets: &[u64]) -> io::Result<()> {
    if offsets.is_empty() {
        return Ok(());
    }
    let (window, window_len) = state
        .window
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODEV))?;
    let mut activated = Vec::new();
    for &offset in offsets {
        if state.active.contains_key(&offset) {
            continue;
        }
        let result = (|| {
            let spec = state.specs[&offset];
            if offset + PAGE_SIZE > window_len as u64 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            // Linux can issue PMD-sized setup requests even when the file ends
            // in the first page. Keep the anonymous zero placeholders beyond
            // EOF instead of manufacturing cache pages for inaccessible bytes.
            if spec.file_offset >= state.cache.store_mut().len(spec.inode)? {
                return Ok(false);
            }
            let flags = if spec.writable {
                MapFlags::READ | MapFlags::WRITE
            } else {
                MapFlags::READ
            };
            let mapping = state
                .cache
                .map_page(spec.inode, spec.file_offset, flags)
                .map_err(io::Error::other)?;
            // KVM registered the DAX window as one writable memslot and may pin
            // every page with write-capable GUP even for guest reads. Keep the
            // HVA writable; the guest PTE still enforces the requested access.
            let protection = libc::PROT_READ | libc::PROT_WRITE;
            // SAFETY: the target is one aligned page inside the owned DAX
            // window, and the backing file contains one complete page.
            let mapped = unsafe {
                libc::mmap(
                    (window + offset as usize) as *mut libc::c_void,
                    PAGE_SIZE as usize,
                    protection,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    mapping.backing().as_raw_fd(),
                    0,
                )
            };
            if mapped == libc::MAP_FAILED {
                let error = io::Error::last_os_error();
                state.cache.unmap(mapping).map_err(io::Error::other)?;
                return Err(error);
            }
            state.active.insert(offset, mapping);
            Ok(true)
        })();
        match result {
            Ok(true) => activated.push(offset),
            Ok(false) => {}
            Err(error) => {
                for offset in activated.into_iter().rev() {
                    let _ = remove_offset(state, offset, false);
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

fn remove_offset(state: &mut State, offset: u64, remove_spec: bool) -> io::Result<()> {
    if let Some(mapping) = state.active.remove(&offset) {
        let (window, _) = state
            .window
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENODEV))?;
        // SAFETY: this replaces one active page in the owned DAX window with a
        // fresh anonymous placeholder while preserving the BAR's host address.
        let mapped = unsafe {
            libc::mmap(
                (window + offset as usize) as *mut libc::c_void,
                PAGE_SIZE as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            state.active.insert(offset, mapping);
            return Err(io::Error::last_os_error());
        }
        state.cache.unmap(mapping).map_err(io::Error::other)?;
    } else if !state.specs.contains_key(&offset) {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    if remove_spec {
        state.specs.remove(&offset);
    }
    Ok(())
}
