// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::collections::HashMap;
use std::io;
use std::mem::{size_of, size_of_val};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock, Weak};

use anyhow::anyhow;
use event_monitor::event;
use seccompiler::SeccompAction;
use serde::{Deserialize, Serialize};
use serde_with::{Bytes, serde_as};
use virtio_queue::{Queue, QueueT};
use vm_device::UserspaceMapping;
use vm_memory::{
    ByteValued, Bytes as VmMemoryBytes, GuestAddressSpace, GuestMemoryAtomic, VolatileMemory,
};
use vm_migration::{Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable};
use vm_virtio::checked_descriptor::DescriptorChainExt;
use vmm_sys_util::eventfd::EventFd;

use crate::device::ActivationContext;
use crate::seccomp_filters::Thread;
use crate::{
    ActivateResult, EPOLL_HELPER_EVENT_LAST, EpollHelper, EpollHelperError, EpollHelperHandler,
    Error as DeviceError, GuestMemoryMmap, VIRTIO_F_VERSION_1, VirtioCommon, VirtioDevice,
    VirtioDeviceType, VirtioInterrupt, VirtioInterruptType, VirtioSharedMemoryList,
};

const QUEUE_SIZE: u16 = 1024;
const QUEUE_SIZES: &[u16] = &[QUEUE_SIZE, QUEUE_SIZE];
const QUEUE_EVENT_BASE: u16 = EPOLL_HELPER_EVENT_LAST + 1;
const ROOT_ID: u64 = 1;
const MAX_BUFFER_SIZE: usize = 1 << 20;

const FUSE_ASYNC_READ: u64 = 1 << 0;
const FUSE_BIG_WRITES: u64 = 1 << 5;
const FUSE_AUTO_INVAL_DATA: u64 = 1 << 12;
const FUSE_PARALLEL_DIROPS: u64 = 1 << 18;
const FUSE_MAX_PAGES: u64 = 1 << 22;
const FUSE_INIT_EXT: u64 = 1 << 30;

const FUSE_LOOKUP: u32 = 1;
const FUSE_FORGET: u32 = 2;
const FUSE_GETATTR: u32 = 3;
const FUSE_OPEN: u32 = 14;
const FUSE_READ: u32 = 15;
const FUSE_WRITE: u32 = 16;
const FUSE_STATFS: u32 = 17;
const FUSE_RELEASE: u32 = 18;
const FUSE_FSYNC: u32 = 20;
const FUSE_FLUSH: u32 = 25;
const FUSE_INIT: u32 = 26;
const FUSE_OPENDIR: u32 = 27;
const FUSE_READDIR: u32 = 28;
const FUSE_RELEASEDIR: u32 = 29;
const FUSE_FSYNCDIR: u32 = 30;
const FUSE_ACCESS: u32 = 34;
const FUSE_DESTROY: u32 = 38;
const FUSE_SETUPMAPPING: u32 = 48;
const FUSE_REMOVEMAPPING: u32 = 49;

const FUSE_SETUPMAPPING_FLAG_WRITE: u64 = 1;
const DT_DIR: u32 = 4;
const DT_REG: u32 = 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct FsNodeAttr {
    pub inode: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
    pub mode: u32,
    pub links: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug)]
pub struct FsDirEntry {
    pub inode: u64,
    pub name: String,
    pub directory: bool,
}

/// Operations needed by the embedded virtio-fs transport.
pub trait InProcessFsBackend: Send + Sync {
    fn lookup(&self, parent: u64, name: &str) -> io::Result<FsNodeAttr>;
    fn getattr(&self, inode: u64) -> io::Result<FsNodeAttr>;
    fn open(&self, inode: u64) -> io::Result<()>;
    fn close(&self, inode: u64) -> io::Result<()>;
    fn read(&self, inode: u64, offset: u64, data: &mut [u8]) -> io::Result<usize>;
    fn write(&self, inode: u64, offset: u64, data: &[u8]) -> io::Result<usize>;
    fn sync(&self, inode: u64) -> io::Result<()>;
    fn entries(&self, inode: u64) -> io::Result<Vec<FsDirEntry>>;
    fn attach_dax_window(&self, host_address: usize, len: usize) -> io::Result<()>;
    fn detach_dax_window(&self, host_address: usize, len: usize) -> io::Result<()>;
    fn setup_mapping(
        &self,
        inode: u64,
        file_offset: u64,
        len: u64,
        writable: bool,
        window_offset: u64,
    ) -> io::Result<()>;
    fn remove_mapping(&self, window_offset: u64, len: u64) -> io::Result<()>;
}

type Backend = dyn InProcessFsBackend;

fn registry() -> &'static Mutex<HashMap<u64, Weak<Backend>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, Weak<Backend>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_in_process_fs(backend: &Arc<Backend>) -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    registry()
        .lock()
        .unwrap()
        .insert(id, Arc::downgrade(backend));
    id
}

pub fn in_process_fs(id: u64) -> Option<Arc<Backend>> {
    registry().lock().unwrap().get(&id)?.upgrade()
}

#[serde_as]
#[repr(C, packed)]
#[derive(Clone, Copy, Deserialize, Serialize)]
pub struct VirtioFsConfig {
    #[serde_as(as = "Bytes")]
    pub tag: [u8; 36],
    pub num_request_queues: u32,
}

// SAFETY: the configuration is only integers and byte arrays.
unsafe impl ByteValued for VirtioFsConfig {}

impl VirtioFsConfig {
    fn new(tag: &str) -> Self {
        let mut config = Self {
            tag: [0; 36],
            num_request_queues: 1,
        };
        let bytes = tag.as_bytes();
        let len = bytes.len().min(config.tag.len());
        config.tag[..len].copy_from_slice(&bytes[..len]);
        config
    }
}

#[derive(Deserialize, Serialize)]
pub struct State {
    avail_features: u64,
    acked_features: u64,
    config: VirtioFsConfig,
}

pub struct InProcessFs {
    common: VirtioCommon,
    id: String,
    config: VirtioFsConfig,
    backend: Arc<Backend>,
    shared_memory: VirtioSharedMemoryList,
    seccomp_action: SeccompAction,
    exit_evt: EventFd,
}

impl InProcessFs {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        tag: &str,
        backend: Arc<Backend>,
        shared_memory: VirtioSharedMemoryList,
        seccomp_action: SeccompAction,
        exit_evt: EventFd,
        state: Option<State>,
    ) -> io::Result<Self> {
        backend.attach_dax_window(
            shared_memory.mapping.as_ptr() as usize,
            shared_memory.mapping.len(),
        )?;
        let (avail_features, acked_features, config, paused) = if let Some(state) = state {
            (
                state.avail_features,
                state.acked_features,
                state.config,
                true,
            )
        } else {
            (
                1u64 << VIRTIO_F_VERSION_1,
                0,
                VirtioFsConfig::new(tag),
                false,
            )
        };
        Ok(Self {
            common: VirtioCommon {
                device_type: VirtioDeviceType::Fs as u32,
                queue_sizes: QUEUE_SIZES.to_vec(),
                paused_sync: Some(Arc::new(Barrier::new(2))),
                avail_features,
                acked_features,
                min_queues: 2,
                paused: Arc::new(AtomicBool::new(paused)),
                ..Default::default()
            },
            id,
            config,
            backend,
            shared_memory,
            seccomp_action,
            exit_evt,
        })
    }

    fn state(&self) -> State {
        State {
            avail_features: self.common.avail_features,
            acked_features: self.common.acked_features,
            config: self.config,
        }
    }
}

impl Drop for InProcessFs {
    fn drop(&mut self) {
        let _ = self.backend.detach_dax_window(
            self.shared_memory.mapping.as_ptr() as usize,
            self.shared_memory.mapping.len(),
        );
    }
}

impl VirtioDevice for InProcessFs {
    fn device_type(&self) -> u32 {
        self.common.device_type
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.common.queue_sizes
    }

    fn features(&self) -> u64 {
        self.common.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        self.common.ack_features(value);
    }

    fn config_size(&self) -> Option<u64> {
        Some(size_of::<VirtioFsConfig>() as u64)
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.read_config_from_slice(self.config.as_slice(), offset, data);
    }

    fn activate(&mut self, context: ActivationContext) -> ActivateResult {
        let ActivationContext {
            mem,
            interrupt_cb,
            mut queues,
            device_status,
        } = context;
        for (_, queue, _) in &mut queues {
            queue.set_event_idx(false);
        }
        self.common.activate(&queues, interrupt_cb.clone())?;
        let (kill_evt, pause_evt) = self.common.dup_eventfds()?;
        let mut handler = FsEpollHandler {
            mem,
            queues,
            backend: Arc::clone(&self.backend),
            interrupt_cb: interrupt_cb.clone(),
            kill_evt,
            pause_evt,
        };
        let paused = self.common.paused.clone();
        let paused_sync = self.common.paused_sync.clone();
        self.common.spawn_worker(
            &self.id,
            &self.seccomp_action,
            Thread::VirtioVhostFs,
            &self.exit_evt,
            device_status,
            interrupt_cb,
            move || handler.run(&paused, paused_sync.as_ref().unwrap()),
        )?;
        event!("virtio-device", "activated", "id", &self.id);
        Ok(())
    }

    fn reset(&mut self) {
        self.common.reset();
    }

    fn shutdown(&mut self) {
        self.common.reset();
    }

    fn get_shm_regions(&self) -> Option<VirtioSharedMemoryList> {
        Some(self.shared_memory.clone())
    }

    fn set_shm_regions(
        &mut self,
        shared_memory: VirtioSharedMemoryList,
    ) -> Result<(), DeviceError> {
        let old_address = self.shared_memory.mapping.as_ptr() as usize;
        let old_len = self.shared_memory.mapping.len();
        self.backend
            .detach_dax_window(old_address, old_len)
            .map_err(DeviceError::IoError)?;
        if let Err(error) = self.backend.attach_dax_window(
            shared_memory.mapping.as_ptr() as usize,
            shared_memory.mapping.len(),
        ) {
            let _ = self.backend.attach_dax_window(old_address, old_len);
            return Err(DeviceError::IoError(error));
        }
        self.shared_memory = shared_memory;
        Ok(())
    }

    fn userspace_mappings(&self) -> Vec<UserspaceMapping> {
        vec![UserspaceMapping {
            mem_slot: self.shared_memory.mem_slot,
            addr: self.shared_memory.addr,
            mapping: Arc::clone(&self.shared_memory.mapping),
            mergeable: false,
        }]
    }
}

impl Pausable for InProcessFs {
    fn pause(&mut self) -> Result<(), MigratableError> {
        self.common.pause()
    }

    fn resume(&mut self) -> Result<(), MigratableError> {
        self.common.resume()
    }
}

impl Snapshottable for InProcessFs {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn snapshot(&mut self) -> Result<Snapshot, MigratableError> {
        Snapshot::new_from_state(&self.state())
    }
}

impl Transportable for InProcessFs {}
impl Migratable for InProcessFs {}

struct FsEpollHandler {
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    queues: Vec<(u16, Queue, EventFd)>,
    backend: Arc<Backend>,
    interrupt_cb: Arc<dyn VirtioInterrupt>,
    kill_evt: EventFd,
    pause_evt: EventFd,
}

impl FsEpollHandler {
    fn run(&mut self, paused: &AtomicBool, paused_sync: &Barrier) -> Result<(), EpollHelperError> {
        let mut helper = EpollHelper::new(&self.kill_evt, &self.pause_evt)?;
        for (index, (_, _, event)) in self.queues.iter().enumerate() {
            helper.add_event(event.as_raw_fd(), QUEUE_EVENT_BASE + index as u16)?;
        }
        helper.run(paused, paused_sync, self)
    }

    fn process_queue(&mut self, index: usize) -> io::Result<bool> {
        let (_, queue, _) = &mut self.queues[index];
        let mut used = false;
        while let Some(mut chain) = queue.pop_descriptor_chain(self.mem.memory()) {
            let descriptors = chain
                .checked_iter(None)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|address| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid descriptor address {address:?}"),
                    )
                })?;
            let mut request = Vec::new();
            for descriptor in descriptors
                .iter()
                .filter(|descriptor| !descriptor.is_write_only())
            {
                let old_len = request.len();
                request.resize(old_len + descriptor.len() as usize, 0);
                chain
                    .memory()
                    .read_slice(&mut request[old_len..], descriptor.addr())
                    .map_err(io::Error::other)?;
            }
            let response = handle_request(self.backend.as_ref(), &request);
            let mut written = 0usize;
            for descriptor in descriptors
                .iter()
                .filter(|descriptor| descriptor.is_write_only())
            {
                if written == response.len() {
                    break;
                }
                let count = (descriptor.len() as usize).min(response.len() - written);
                chain
                    .memory()
                    .write_slice(&response[written..written + count], descriptor.addr())
                    .map_err(io::Error::other)?;
                written += count;
            }
            queue
                .add_used(chain.memory(), chain.head_index(), written as u32)
                .map_err(io::Error::other)?;
            used = true;
        }
        if used {
            queue
                .needs_notification(&*self.mem.memory())
                .map_err(io::Error::other)
        } else {
            Ok(false)
        }
    }

    fn process_and_signal(&mut self, index: usize) -> Result<(), EpollHelperError> {
        if self
            .process_queue(index)
            .map_err(|error| EpollHelperError::HandleEvent(error.into()))?
        {
            self.signal_queue(index)?;
        }
        Ok(())
    }

    fn signal_queue(&self, index: usize) -> Result<(), EpollHelperError> {
        let queue_id = self.queues[index].0;
        self.interrupt_cb
            .trigger(VirtioInterruptType::Queue(queue_id))
            .map_err(|error| EpollHelperError::HandleEvent(error.into()))
    }
}

impl EpollHelperHandler for FsEpollHandler {
    fn handle_event(
        &mut self,
        _helper: &mut EpollHelper,
        event: &epoll::Event,
    ) -> Result<(), EpollHelperError> {
        let index = (event.data as u16)
            .checked_sub(QUEUE_EVENT_BASE)
            .map(usize::from)
            .filter(|index| *index < self.queues.len())
            .ok_or_else(|| EpollHelperError::HandleEvent(anyhow!("unexpected fs event")))?;
        self.queues[index]
            .2
            .read()
            .map_err(|error| EpollHelperError::HandleEvent(error.into()))?;
        self.process_and_signal(index)
    }
}

fn handle_request(backend: &Backend, request: &[u8]) -> Vec<u8> {
    let Ok((header, _)) = read_struct::<InHeader>(request) else {
        return Vec::new();
    };
    if header.len as usize > request.len() || (header.len as usize) < size_of::<InHeader>() {
        return reply_error(header.unique, libc::EINVAL);
    }
    let body = &request[size_of::<InHeader>()..header.len as usize];

    let result = match header.opcode {
        FUSE_LOOKUP => {
            let name = body
                .iter()
                .position(|byte| *byte == 0)
                .filter(|length| *length > 0)
                .and_then(|length| std::str::from_utf8(&body[..length]).ok())
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL));
            name.and_then(|name| backend.lookup(header.nodeid, name))
                .map(|attr| struct_reply(header.unique, EntryOut::from(attr)))
        }
        FUSE_GETATTR => backend
            .getattr(header.nodeid)
            .map(|attr| struct_reply(header.unique, AttrOut::from(attr))),
        FUSE_OPEN | FUSE_OPENDIR => backend.open(header.nodeid).map(|()| {
            struct_reply(
                header.unique,
                OpenOut {
                    fh: header.nodeid,
                    open_flags: 0,
                    padding: 0,
                },
            )
        }),
        FUSE_READ => read_struct::<ReadIn>(body).and_then(|(input, _)| {
            let mut data = vec![0; input.size.min(MAX_BUFFER_SIZE as u32) as usize];
            backend
                .read(header.nodeid, input.offset, &mut data)
                .map(|count| data_reply(header.unique, &data[..count]))
        }),
        FUSE_WRITE => read_struct::<WriteIn>(body).and_then(|(input, data)| {
            let count = (input.size as usize).min(data.len());
            backend
                .write(header.nodeid, input.offset, &data[..count])
                .map(|written| {
                    struct_reply(
                        header.unique,
                        WriteOut {
                            size: written as u32,
                            padding: 0,
                        },
                    )
                })
        }),
        FUSE_RELEASE | FUSE_RELEASEDIR => {
            let sync = backend.sync(header.nodeid);
            let close = backend.close(header.nodeid);
            sync.and(close).map(|()| empty_reply(header.unique))
        }
        FUSE_FSYNC | FUSE_FSYNCDIR | FUSE_FLUSH => backend
            .sync(header.nodeid)
            .map(|()| empty_reply(header.unique)),
        FUSE_STATFS => Ok(struct_reply(header.unique, StatfsOut::default())),
        FUSE_INIT => init_reply(header.unique, body),
        FUSE_READDIR => read_struct::<ReadIn>(body).and_then(|(input, _)| {
            backend.entries(header.nodeid).map(|entries| {
                let data = directory_reply(header.nodeid, input.offset, input.size, entries);
                data_reply(header.unique, &data)
            })
        }),
        FUSE_ACCESS => Ok(empty_reply(header.unique)),
        FUSE_SETUPMAPPING => read_struct::<SetupMappingIn>(body).and_then(|(input, _)| {
            backend
                .setup_mapping(
                    header.nodeid,
                    input.file_offset,
                    input.len,
                    input.flags & FUSE_SETUPMAPPING_FLAG_WRITE != 0,
                    input.window_offset,
                )
                .map(|()| empty_reply(header.unique))
        }),
        FUSE_REMOVEMAPPING => remove_mapping(backend, header.unique, body),
        FUSE_FORGET | FUSE_DESTROY => return Vec::new(),
        _ => Err(io::Error::from_raw_os_error(libc::ENOSYS)),
    };
    result.unwrap_or_else(|error| {
        reply_error(
            header.unique,
            error.raw_os_error().unwrap_or(libc::EIO).unsigned_abs() as i32,
        )
    })
}

fn init_reply(unique: u64, body: &[u8]) -> io::Result<Vec<u8>> {
    let (input, rest) = read_struct::<InitIn>(body)?;
    if input.major != 7 {
        return Ok(struct_reply(
            unique,
            InitOut {
                major: 7,
                minor: 38,
                ..Default::default()
            },
        ));
    }
    let flags2 = if input.flags as u64 & FUSE_INIT_EXT != 0 {
        read_struct::<InitInExt>(rest).map(|(ext, _)| ext.flags2)?
    } else {
        0
    };
    let capable = input.flags as u64 | ((flags2 as u64) << 32);
    let supported = FUSE_ASYNC_READ
        | FUSE_BIG_WRITES
        | FUSE_AUTO_INVAL_DATA
        | FUSE_PARALLEL_DIROPS
        | FUSE_MAX_PAGES
        | FUSE_INIT_EXT;
    let enabled = capable & supported;
    Ok(struct_reply(
        unique,
        InitOut {
            major: 7,
            minor: 38,
            max_readahead: input.max_readahead,
            flags: enabled as u32,
            max_background: u16::MAX,
            congestion_threshold: (u16::MAX / 4) * 3,
            max_write: MAX_BUFFER_SIZE as u32,
            time_gran: 1,
            max_pages: 256,
            map_alignment: 0,
            flags2: (enabled >> 32) as u32,
            unused: [0; 7],
        },
    ))
}

fn remove_mapping(backend: &Backend, unique: u64, mut body: &[u8]) -> io::Result<Vec<u8>> {
    let (input, rest) = read_struct::<RemoveMappingIn>(body)?;
    body = rest;
    for _ in 0..input.count {
        let (mapping, rest) = read_struct::<RemoveMappingOne>(body)?;
        backend.remove_mapping(mapping.window_offset, mapping.len)?;
        body = rest;
    }
    Ok(empty_reply(unique))
}

fn directory_reply(parent: u64, offset: u64, size: u32, entries: Vec<FsDirEntry>) -> Vec<u8> {
    let mut all = vec![
        FsDirEntry {
            inode: parent,
            name: ".".into(),
            directory: true,
        },
        FsDirEntry {
            inode: ROOT_ID,
            name: "..".into(),
            directory: true,
        },
    ];
    all.extend(entries);
    let mut output = Vec::new();
    for (index, entry) in all.into_iter().enumerate().skip(offset as usize) {
        let dirent = Dirent {
            inode: entry.inode,
            offset: (index + 1) as u64,
            name_len: entry.name.len() as u32,
            kind: if entry.directory { DT_DIR } else { DT_REG },
        };
        let record_len = (size_of::<Dirent>() + entry.name.len() + 7) & !7;
        if output.len() + record_len > size as usize {
            break;
        }
        append_struct(&mut output, &dirent);
        output.extend_from_slice(entry.name.as_bytes());
        output.resize(
            output.len() + record_len - size_of::<Dirent>() - entry.name.len(),
            0,
        );
    }
    output
}

fn empty_reply(unique: u64) -> Vec<u8> {
    struct_reply(unique, Empty {})
}

fn data_reply(unique: u64, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(size_of::<OutHeader>() + data.len());
    append_struct(
        &mut output,
        &OutHeader {
            len: (size_of::<OutHeader>() + data.len()) as u32,
            error: 0,
            unique,
        },
    );
    output.extend_from_slice(data);
    output
}

fn struct_reply<T>(unique: u64, value: T) -> Vec<u8> {
    let mut output = Vec::with_capacity(size_of::<OutHeader>() + size_of::<T>());
    append_struct(
        &mut output,
        &OutHeader {
            len: (size_of::<OutHeader>() + size_of::<T>()) as u32,
            error: 0,
            unique,
        },
    );
    append_struct(&mut output, &value);
    output
}

fn reply_error(unique: u64, errno: i32) -> Vec<u8> {
    let mut output = Vec::with_capacity(size_of::<OutHeader>());
    append_struct(
        &mut output,
        &OutHeader {
            len: size_of::<OutHeader>() as u32,
            error: -errno.abs(),
            unique,
        },
    );
    output
}

fn read_struct<T: Copy>(bytes: &[u8]) -> io::Result<(T, &[u8])> {
    if bytes.len() < size_of::<T>() {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: the length check proves the source range, unaligned reads are
    // supported, and every protocol structure here accepts all bit patterns.
    let value = unsafe { bytes.as_ptr().cast::<T>().read_unaligned() };
    Ok((value, &bytes[size_of::<T>()..]))
}

fn append_struct<T>(bytes: &mut Vec<u8>, value: &T) {
    // SAFETY: protocol structures contain no references and the destination
    // copies the bytes before the value can be dropped.
    let raw = unsafe { std::slice::from_raw_parts((value as *const T).cast(), size_of_val(value)) };
    bytes.extend_from_slice(raw);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Empty {}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InHeader {
    len: u32,
    opcode: u32,
    unique: u64,
    nodeid: u64,
    uid: u32,
    gid: u32,
    pid: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OutHeader {
    len: u32,
    error: i32,
    unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Attr {
    inode: u64,
    size: u64,
    blocks: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    atime_nsec: u32,
    mtime_nsec: u32,
    ctime_nsec: u32,
    mode: u32,
    links: u32,
    uid: u32,
    gid: u32,
    rdev: u32,
    block_size: u32,
    flags: u32,
}

impl From<FsNodeAttr> for Attr {
    fn from(value: FsNodeAttr) -> Self {
        Self {
            inode: value.inode,
            size: value.size,
            blocks: value.blocks,
            atime: value.atime.0,
            mtime: value.mtime.0,
            ctime: value.ctime.0,
            atime_nsec: value.atime.1,
            mtime_nsec: value.mtime.1,
            ctime_nsec: value.ctime.1,
            mode: value.mode,
            links: value.links,
            uid: value.uid,
            gid: value.gid,
            rdev: 0,
            block_size: 4096,
            flags: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EntryOut {
    nodeid: u64,
    generation: u64,
    entry_valid: u64,
    attr_valid: u64,
    entry_valid_nsec: u32,
    attr_valid_nsec: u32,
    attr: Attr,
}

impl From<FsNodeAttr> for EntryOut {
    fn from(value: FsNodeAttr) -> Self {
        Self {
            nodeid: value.inode,
            generation: 0,
            entry_valid: 1,
            attr_valid: 1,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: value.into(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AttrOut {
    attr_valid: u64,
    attr_valid_nsec: u32,
    dummy: u32,
    attr: Attr,
}

impl From<FsNodeAttr> for AttrOut {
    fn from(value: FsNodeAttr) -> Self {
        Self {
            attr_valid: 1,
            attr_valid_nsec: 0,
            dummy: 0,
            attr: value.into(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OpenOut {
    fh: u64,
    open_flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ReadIn {
    fh: u64,
    offset: u64,
    size: u32,
    read_flags: u32,
    lock_owner: u64,
    flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WriteIn {
    fh: u64,
    offset: u64,
    size: u32,
    write_flags: u32,
    lock_owner: u64,
    flags: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WriteOut {
    size: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Statfs {
    blocks: u64,
    free_blocks: u64,
    available_blocks: u64,
    files: u64,
    free_files: u64,
    block_size: u32,
    name_len: u32,
    fragment_size: u32,
    padding: u32,
    spare: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StatfsOut {
    stat: Statfs,
}

impl Default for StatfsOut {
    fn default() -> Self {
        Self {
            stat: Statfs {
                blocks: 8192,
                free_blocks: 4096,
                available_blocks: 4096,
                files: 1024,
                free_files: 1024,
                block_size: 4096,
                name_len: 255,
                fragment_size: 4096,
                padding: 0,
                spare: [0; 6],
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InitIn {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InitInExt {
    flags2: u32,
    unused: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InitOut {
    major: u32,
    minor: u32,
    max_readahead: u32,
    flags: u32,
    max_background: u16,
    congestion_threshold: u16,
    max_write: u32,
    time_gran: u32,
    max_pages: u16,
    map_alignment: u16,
    flags2: u32,
    unused: [u32; 7],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SetupMappingIn {
    fh: u64,
    file_offset: u64,
    len: u64,
    flags: u64,
    window_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RemoveMappingIn {
    count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RemoveMappingOne {
    window_offset: u64,
    len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Dirent {
    inode: u64,
    offset: u64,
    name_len: u32,
    kind: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct EmptyBackend {
        close_calls: AtomicUsize,
        lookup_calls: AtomicUsize,
        sync_error: bool,
    }

    impl InProcessFsBackend for EmptyBackend {
        fn lookup(&self, _parent: u64, _name: &str) -> io::Result<FsNodeAttr> {
            self.lookup_calls.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::from_raw_os_error(libc::ENOENT))
        }
        fn getattr(&self, inode: u64) -> io::Result<FsNodeAttr> {
            Ok(FsNodeAttr {
                inode,
                mode: libc::S_IFDIR | 0o755,
                links: 2,
                ..Default::default()
            })
        }
        fn open(&self, _inode: u64) -> io::Result<()> {
            Ok(())
        }
        fn close(&self, _inode: u64) -> io::Result<()> {
            self.close_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn read(&self, _inode: u64, _offset: u64, _data: &mut [u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn write(&self, _inode: u64, _offset: u64, data: &[u8]) -> io::Result<usize> {
            Ok(data.len())
        }
        fn sync(&self, _inode: u64) -> io::Result<()> {
            if self.sync_error {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }
        fn entries(&self, _inode: u64) -> io::Result<Vec<FsDirEntry>> {
            Ok(Vec::new())
        }
        fn attach_dax_window(&self, _host_address: usize, _len: usize) -> io::Result<()> {
            Ok(())
        }
        fn detach_dax_window(&self, _host_address: usize, _len: usize) -> io::Result<()> {
            Ok(())
        }
        fn setup_mapping(
            &self,
            _inode: u64,
            _file_offset: u64,
            _len: u64,
            _writable: bool,
            _window_offset: u64,
        ) -> io::Result<()> {
            Ok(())
        }
        fn remove_mapping(&self, _window_offset: u64, _len: u64) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn fuse_init_negotiates_supported_transport_features() {
        let backend = EmptyBackend::default();
        let header = InHeader {
            len: (size_of::<InHeader>() + size_of::<InitIn>() + size_of::<InitInExt>()) as u32,
            opcode: FUSE_INIT,
            unique: 7,
            nodeid: ROOT_ID,
            ..Default::default()
        };
        let input = InitIn {
            major: 7,
            minor: 38,
            max_readahead: 4096,
            flags: (FUSE_INIT_EXT | FUSE_BIG_WRITES) as u32,
        };
        let extension = InitInExt {
            flags2: 0,
            ..Default::default()
        };
        let mut request = Vec::new();
        append_struct(&mut request, &header);
        append_struct(&mut request, &input);
        append_struct(&mut request, &extension);

        let response = handle_request(&backend, &request);
        let (_, body) = read_struct::<OutHeader>(&response).unwrap();
        let (output, _) = read_struct::<InitOut>(body).unwrap();
        assert_ne!(output.flags as u64 & FUSE_BIG_WRITES, 0);
        assert_eq!(output.flags2, 0);
    }

    #[test]
    fn request_parser_ignores_bytes_past_the_declared_length() {
        let backend = EmptyBackend::default();
        let header = InHeader {
            len: size_of::<InHeader>() as u32,
            opcode: FUSE_LOOKUP,
            unique: 9,
            nodeid: ROOT_ID,
            ..Default::default()
        };
        let mut request = Vec::new();
        append_struct(&mut request, &header);
        request.extend_from_slice(b"outside-header\0");

        let response = handle_request(&backend, &request);
        let (output, _) = read_struct::<OutHeader>(&response).unwrap();
        assert_eq!(output.error, -libc::EINVAL);
        assert_eq!(backend.lookup_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn release_closes_the_handle_when_sync_fails() {
        let backend = EmptyBackend {
            sync_error: true,
            ..Default::default()
        };
        let header = InHeader {
            len: size_of::<InHeader>() as u32,
            opcode: FUSE_RELEASE,
            unique: 10,
            nodeid: ROOT_ID,
            ..Default::default()
        };
        let mut request = Vec::new();
        append_struct(&mut request, &header);

        let response = handle_request(&backend, &request);
        let (output, _) = read_struct::<OutHeader>(&response).unwrap();
        assert_eq!(output.error, -libc::EIO);
        assert_eq!(backend.close_calls.load(Ordering::Relaxed), 1);
    }
}
