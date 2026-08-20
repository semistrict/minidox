use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloud_hypervisor::vm_config::{FsConfig, PciDeviceCommonConfig};
use cloud_hypervisor_hypervisor::{Hypervisor, HypervisorVmConfig, Vm};
use minidox_cache::{CacheError, CachePageAccounting, NodeId};
use serde::{Deserialize, Serialize};
use vm_migration::Snapshot;
use vm_migration::protocol::MemoryRangeTable;

use crate::ram::RamState;
use crate::virtiofs::{VirtioFsBranch, VirtioFsSnapshot};
use crate::{CloudHypervisorVm, KvmGuestRam, RamAccounting, VmConfig, VmForkStateCapture};

const RAM_SLOT: u32 = 0;
const RAM_GPA: u64 = 0;
const MANIFEST_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct ForestManifest {
    version: u32,
    next_vm: u64,
    next_lineage: u64,
    vms: Vec<VmManifest>,
}

#[derive(Deserialize, Serialize)]
struct VmManifest {
    id: u64,
    backend: DurableBackend,
    ram: RamState,
    filesystem: VirtioFsSnapshot,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DurableBackend {
    Raw,
    Cloud {
        config: Box<VmConfig>,
        snapshot: Snapshot,
        dirty_memory: MemoryRangeTable,
        in_process_filesystems: Vec<usize>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct KvmVmId(u64);

impl KvmVmId {
    pub const fn from_raw(id: u64) -> Self {
        Self(id)
    }

    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

struct SupervisedVm {
    backend: VmBackend,
    ram: KvmGuestRam,
    filesystem: Arc<VirtioFsBranch>,
}

enum VmBackend {
    Raw(Arc<dyn Vm>),
    Cloud(CloudHypervisorVm),
}

impl VmBackend {
    fn pause(&self) -> Result<(), SupervisorError> {
        match self {
            Self::Raw(vm) => vm
                .pause()
                .map_err(|error| SupervisorError::backend("pause KVM VM", error)),
            Self::Cloud(vm) => vm.pause().map_err(Into::into),
        }
    }

    fn resume(&self) -> Result<(), SupervisorError> {
        match self {
            Self::Raw(vm) => vm
                .resume()
                .map_err(|error| SupervisorError::backend("resume KVM VM", error)),
            Self::Cloud(vm) => vm.resume().map_err(Into::into),
        }
    }
}

impl Drop for SupervisedVm {
    fn drop(&mut self) {
        if let VmBackend::Raw(vm) = &self.backend {
            let _ = self.ram.unregister(vm.as_ref(), RAM_SLOT, RAM_GPA);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmForkAccounting {
    pub memory: RamAccounting,
    pub filesystem: CachePageAccounting,
}

/// Owns KVM memory slots and RedoxFS/DAX branches in one fork forest.
pub struct KvmSupervisor {
    hypervisor: Arc<dyn Hypervisor>,
    next_vm: u64,
    next_lineage: u64,
    vms: BTreeMap<KvmVmId, SupervisedVm>,
    last_fork_pause: Option<Duration>,
    storage: Option<PathBuf>,
}

impl KvmSupervisor {
    pub fn new() -> Result<Self, SupervisorError> {
        Ok(Self {
            hypervisor: cloud_hypervisor_hypervisor::new()
                .map_err(|error| SupervisorError::backend("create KVM hypervisor", error))?,
            next_vm: 1,
            next_lineage: 1,
            vms: BTreeMap::new(),
            last_fork_pause: None,
            storage: None,
        })
    }

    /// Open a durable fork forest, restoring its last completed checkpoint.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, SupervisorError> {
        let storage = directory.as_ref().to_path_buf();
        fs::create_dir_all(storage.join("ram/generations"))?;
        fs::create_dir_all(storage.join("filesystems"))?;
        KvmGuestRam::reserve_persisted_generation_ids(&storage.join("ram/generations"))?;
        let manifest_path = storage.join("forest.json");
        if !manifest_path.exists() {
            let mut supervisor = Self::new()?;
            supervisor.storage = Some(storage);
            return Ok(supervisor);
        }

        let manifest: ForestManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
        if manifest.version != MANIFEST_VERSION {
            return Err(SupervisorError::UnsupportedManifestVersion(
                manifest.version,
            ));
        }

        let hypervisor = cloud_hypervisor_hypervisor::new()
            .map_err(|error| SupervisorError::backend("create KVM hypervisor", error))?;
        let rams = KvmGuestRam::restore(
            manifest.vms.iter().map(|vm| vm.ram.clone()).collect(),
            &storage.join("ram/generations"),
        )?;

        let mut by_lineage = BTreeMap::<u64, Vec<(usize, VirtioFsSnapshot)>>::new();
        for (index, vm) in manifest.vms.iter().enumerate() {
            by_lineage
                .entry(vm.filesystem.lineage)
                .or_default()
                .push((index, vm.filesystem.clone()));
        }
        let mut filesystems = vec![None; manifest.vms.len()];
        for (lineage, entries) in by_lineage {
            let indexes = entries.iter().map(|(index, _)| *index).collect::<Vec<_>>();
            let snapshots = entries.into_iter().map(|(_, snapshot)| snapshot).collect();
            let restored = VirtioFsBranch::restore_lineage(
                storage.join("filesystems").join(lineage.to_string()),
                snapshots,
            )?;
            for (index, filesystem) in indexes.into_iter().zip(restored) {
                filesystems[index] = Some(filesystem);
            }
        }

        let mut vms = BTreeMap::new();
        for ((saved, ram), filesystem) in manifest.vms.into_iter().zip(rams).zip(filesystems) {
            let filesystem = filesystem.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing restored filesystem branch",
                )
            })?;
            let backend = match saved.backend {
                DurableBackend::Raw => {
                    let vm = hypervisor
                        .create_vm(HypervisorVmConfig::default())
                        .map_err(|error| SupervisorError::backend("restore KVM VM", error))?;
                    ram.register(vm.as_ref(), RAM_SLOT, RAM_GPA)?;
                    VmBackend::Raw(vm)
                }
                DurableBackend::Cloud {
                    mut config,
                    snapshot,
                    dirty_memory,
                    in_process_filesystems,
                } => {
                    if let Some(configured) = config.fs.as_mut() {
                        for index in in_process_filesystems {
                            let item = configured.get_mut(index).ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "in-process filesystem index is outside VM config",
                                )
                            })?;
                            item.in_process_backend_id = Some(filesystem.backend_id());
                            item.dax_window_size = filesystem.dax_window_size();
                        }
                    }
                    let vm = CloudHypervisorVm::start()?;
                    vm.restore_fork_state(
                        VmForkStateCapture {
                            config,
                            snapshot,
                            dirty_memory,
                            dirty_ram_pages: Vec::new(),
                        },
                        &ram,
                    )?;
                    vm.begin_fork_tracking()?;
                    vm.resume()?;
                    VmBackend::Cloud(vm)
                }
            };
            vms.insert(
                KvmVmId(saved.id),
                SupervisedVm {
                    backend,
                    ram,
                    filesystem,
                },
            );
        }

        Ok(Self {
            hypervisor,
            next_vm: manifest.next_vm,
            next_lineage: manifest.next_lineage,
            vms,
            last_fork_pause: None,
            storage: Some(storage),
        })
    }

    pub fn create_vm(&mut self, ram_size: usize) -> Result<KvmVmId, SupervisorError> {
        let vm = self
            .hypervisor
            .create_vm(HypervisorVmConfig::default())
            .map_err(|error| SupervisorError::backend("create KVM VM", error))?;
        let ram = KvmGuestRam::new(ram_size)?;
        ram.register(vm.as_ref(), RAM_SLOT, RAM_GPA)?;
        let filesystem = match self.create_filesystem() {
            Ok(filesystem) => filesystem,
            Err(error) => {
                let _ = ram.unregister(vm.as_ref(), RAM_SLOT, RAM_GPA);
                return Err(error.into());
            }
        };
        let id = KvmVmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            id,
            SupervisedVm {
                backend: VmBackend::Raw(vm),
                ram,
                filesystem,
            },
        );
        Ok(id)
    }

    /// Boot a Cloud Hypervisor VM over supervisor-owned CoW RAM.
    pub fn create_cloud_vm(&mut self, mut config: VmConfig) -> Result<KvmVmId, SupervisorError> {
        let ram_size = usize::try_from(config.memory.size)
            .map_err(|_| crate::Error::InvalidRamSize(usize::MAX))?;
        let ram = KvmGuestRam::new(ram_size)?;
        let filesystem = self.create_filesystem()?;
        config.fs.get_or_insert_with(Vec::new).push(FsConfig {
            pci_common: PciDeviceCommonConfig::default(),
            tag: "minidox".to_owned(),
            socket: PathBuf::new(),
            num_queues: 1,
            queue_size: 1024,
            in_process_backend_id: Some(filesystem.backend_id()),
            dax_window_size: filesystem.dax_window_size(),
        });
        let backend = CloudHypervisorVm::start()?;
        backend.create(config)?;
        backend.boot_with_ram(&ram)?;

        let id = KvmVmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            id,
            SupervisedVm {
                backend: VmBackend::Cloud(backend),
                ram,
                filesystem,
            },
        );
        Ok(id)
    }

    pub fn create_file(
        &mut self,
        vm: KvmVmId,
        name: &str,
        size: u64,
    ) -> Result<NodeId, SupervisorError> {
        let vm = self.vm_mut(vm)?;
        vm.backend.pause()?;
        let create_result: Result<NodeId, SupervisorError> = vm
            .filesystem
            .create_file(name, size)
            .map_err(SupervisorError::from);
        let resume_result = vm.backend.resume();
        let node = create_result?;
        resume_result?;
        Ok(node)
    }

    /// Publish the source's RAM and filesystem roots under one barrier.
    ///
    /// The Cloud Hypervisor lifecycle adapter must first stop its vCPU and
    /// device workers so none can enter `KVM_RUN` or mutate DAX during this
    /// call. The raw hypervisor VM pause hook only brackets hypervisor state;
    /// it does not own those worker threads.
    pub fn fork_vm(&mut self, source: KvmVmId) -> Result<KvmVmId, SupervisorError> {
        if matches!(&self.vm(source)?.backend, VmBackend::Cloud(_)) {
            return self.fork_cloud_vm(source);
        }

        let child_vm = self
            .hypervisor
            .create_vm(HypervisorVmConfig::default())
            .map_err(|error| SupervisorError::backend("create child KVM VM", error))?;
        let source_vm = self.vm_mut(source)?;
        let pause_started = Instant::now();
        let VmBackend::Raw(source_backend) = &source_vm.backend else {
            unreachable!("backend kind was checked above")
        };
        source_backend
            .pause()
            .map_err(|error| SupervisorError::backend("pause source KVM VM", error))?;

        let fork_result = (|| {
            let child_filesystem = source_vm.filesystem.fork()?;
            let child_ram = source_vm
                .ram
                .fork(source_backend.as_ref(), RAM_SLOT, RAM_GPA)?;
            child_ram.register(child_vm.as_ref(), RAM_SLOT, RAM_GPA)?;
            Ok::<_, SupervisorError>((child_ram, child_filesystem))
        })();

        let resume_result = source_backend
            .resume()
            .map_err(|error| SupervisorError::backend("resume source KVM VM", error));
        let (child_ram, child_filesystem) = fork_result?;
        if let Err(error) = resume_result {
            let _ = child_ram.unregister(child_vm.as_ref(), RAM_SLOT, RAM_GPA);
            return Err(error);
        }
        let pause_duration = pause_started.elapsed();

        let child = KvmVmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            child,
            SupervisedVm {
                backend: VmBackend::Raw(child_vm),
                ram: child_ram,
                filesystem: child_filesystem,
            },
        );
        self.last_fork_pause = Some(pause_duration);
        Ok(child)
    }

    fn fork_cloud_vm(&mut self, source: KvmVmId) -> Result<KvmVmId, SupervisorError> {
        let source_vm = self.vm_mut(source)?;
        let VmBackend::Cloud(source_backend) = &source_vm.backend else {
            unreachable!("backend kind was checked by fork_vm")
        };
        let dirty_before_copy = source_backend.take_dirty_ram_pages()?;
        let preparation = source_vm.ram.prepare_fork(&dirty_before_copy)?;

        let pause_started = Instant::now();
        source_backend.pause()?;

        let fork_result = (|| {
            let mut capture = source_backend.capture_fork_state()?;
            let child_filesystem = source_vm.filesystem.fork()?;
            if let Some(filesystems) = capture.config.fs.as_mut() {
                for filesystem in filesystems {
                    if filesystem.in_process_backend_id.is_some() {
                        filesystem.in_process_backend_id = Some(child_filesystem.backend_id());
                        filesystem.dax_window_size = child_filesystem.dax_window_size();
                    }
                }
            }
            let child_ram_branch = source_vm
                .ram
                .finish_prepared_branch(preparation, &capture.dirty_ram_pages)?;
            Ok::<_, SupervisorError>((capture, child_ram_branch, child_filesystem))
        })();

        let resume_result = source_backend.resume().map_err(SupervisorError::from);
        let (capture, child_ram_branch, child_filesystem) = fork_result?;
        resume_result?;
        let pause_duration = pause_started.elapsed();

        let child_ram = child_ram_branch.materialize()?;
        let child_backend = CloudHypervisorVm::start()?;
        child_backend.restore_fork_state(capture, &child_ram)?;
        child_backend.begin_fork_tracking()?;
        child_backend.resume()?;

        let child = KvmVmId(self.next_vm);
        self.next_vm += 1;
        self.vms.insert(
            child,
            SupervisedVm {
                backend: VmBackend::Cloud(child_backend),
                ram: child_ram,
                filesystem: child_filesystem,
            },
        );
        self.last_fork_pause = Some(pause_duration);
        Ok(child)
    }

    pub fn remove_vm(&mut self, vm: KvmVmId) -> Result<(), SupervisorError> {
        self.vms
            .remove(&vm)
            .map(drop)
            .ok_or(SupervisorError::VmNotFound(vm))
    }

    /// Atomically publish the current durable RAM and RedoxFS roots.
    pub fn checkpoint(&mut self) -> Result<(), SupervisorError> {
        let storage = self
            .storage
            .clone()
            .ok_or(SupervisorError::DurabilityRequired)?;

        let mut paused = Vec::with_capacity(self.vms.len());
        for (&id, vm) in &self.vms {
            if let Err(error) = vm.backend.pause() {
                for paused_id in paused.into_iter().rev() {
                    let _ = self.vms[&paused_id].backend.resume();
                }
                return Err(error);
            }
            paused.push(id);
        }

        let checkpoint_result = (|| {
            let mut saved = Vec::with_capacity(self.vms.len());
            for (&id, vm) in &mut self.vms {
                let backend = match &vm.backend {
                    VmBackend::Raw(raw) => {
                        vm.ram
                            .seal_for_checkpoint(raw.as_ref(), RAM_SLOT, RAM_GPA)?;
                        DurableBackend::Raw
                    }
                    VmBackend::Cloud(cloud) => {
                        let capture = cloud.capture_fork_state()?;
                        vm.ram.seal_dirty_pages(&capture.dirty_ram_pages)?;
                        let in_process_filesystems = capture
                            .config
                            .fs
                            .iter()
                            .flatten()
                            .enumerate()
                            .filter_map(|(index, filesystem)| {
                                filesystem.in_process_backend_id.map(|_| index)
                            })
                            .collect();
                        DurableBackend::Cloud {
                            config: capture.config,
                            snapshot: capture.snapshot,
                            dirty_memory: capture.dirty_memory,
                            in_process_filesystems,
                        }
                    }
                };
                let filesystem = vm.filesystem.durable_snapshot()?;
                let ram = vm.ram.durable_state(&storage.join("ram/generations"))?;
                saved.push(VmManifest {
                    id: id.0,
                    backend,
                    ram,
                    filesystem,
                });
            }
            Ok::<_, SupervisorError>(ForestManifest {
                version: MANIFEST_VERSION,
                next_vm: self.next_vm,
                next_lineage: self.next_lineage,
                vms: saved,
            })
        })();

        let mut resume_result = Ok(());
        for id in paused.into_iter().rev() {
            if let Err(error) = self.vms[&id].backend.resume()
                && resume_result.is_ok()
            {
                resume_result = Err(error);
            }
        }
        let manifest = checkpoint_result?;
        resume_result?;
        publish_manifest(&storage, &manifest)
    }

    pub fn write_memory(
        &mut self,
        vm: KvmVmId,
        guest_address: usize,
        bytes: &[u8],
    ) -> Result<(), SupervisorError> {
        let vm = self.vm_mut(vm)?;
        vm.backend.pause()?;
        let write_result = vm
            .ram
            .write(guest_address, bytes)
            .map_err(SupervisorError::from);
        let resume_result = vm.backend.resume();
        write_result?;
        resume_result
    }

    pub fn read_memory(
        &self,
        vm: KvmVmId,
        guest_address: usize,
        len: usize,
    ) -> Result<Vec<u8>, SupervisorError> {
        let vm = self.vm(vm)?;
        vm.backend.pause()?;
        let mut bytes = vec![0; len];
        let read_result = vm
            .ram
            .read(guest_address, &mut bytes)
            .map_err(SupervisorError::from);
        let resume_result = vm.backend.resume();
        read_result?;
        resume_result?;
        Ok(bytes)
    }

    pub fn write_file(
        &mut self,
        vm: KvmVmId,
        node: NodeId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), SupervisorError> {
        let vm = self.vm_mut(vm)?;
        vm.backend.pause()?;
        let result: Result<(), SupervisorError> = vm
            .filesystem
            .write(node, offset, bytes)
            .map_err(SupervisorError::from);
        let resume_result = vm.backend.resume();
        result?;
        resume_result?;
        Ok(())
    }

    pub fn read_file(
        &mut self,
        vm: KvmVmId,
        node: NodeId,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, SupervisorError> {
        let vm = self.vm_mut(vm)?;
        vm.backend.pause()?;
        let result: Result<Vec<u8>, SupervisorError> = vm
            .filesystem
            .read(node, offset, len)
            .map_err(SupervisorError::from);
        let resume_result = vm.backend.resume();
        let bytes = result?;
        resume_result?;
        Ok(bytes)
    }

    pub fn page_accounting(&self) -> KvmForkAccounting {
        KvmForkAccounting {
            memory: KvmGuestRam::page_accounting(self.vms.values().map(|vm| &vm.ram)),
            filesystem: VirtioFsBranch::page_accounting(self.vms.values().map(|vm| &vm.filesystem)),
        }
    }

    pub fn last_fork_pause(&self) -> Option<Duration> {
        self.last_fork_pause
    }

    pub fn vm_ids(&self) -> Vec<KvmVmId> {
        self.vms.keys().copied().collect()
    }

    fn create_filesystem(&mut self) -> Result<Arc<VirtioFsBranch>, SupervisorError> {
        let Some(storage) = &self.storage else {
            return VirtioFsBranch::create().map_err(Into::into);
        };
        let lineage = self.next_lineage;
        self.next_lineage += 1;
        VirtioFsBranch::create_durable(
            lineage,
            storage.join("filesystems").join(lineage.to_string()),
        )
        .map_err(Into::into)
    }

    fn vm(&self, id: KvmVmId) -> Result<&SupervisedVm, SupervisorError> {
        self.vms.get(&id).ok_or(SupervisorError::VmNotFound(id))
    }

    fn vm_mut(&mut self, id: KvmVmId) -> Result<&mut SupervisedVm, SupervisorError> {
        self.vms.get_mut(&id).ok_or(SupervisorError::VmNotFound(id))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("VM {0:?} does not exist")]
    VmNotFound(KvmVmId),
    #[error(transparent)]
    Vmm(#[from] crate::Error),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("manifest version {0} is not supported")]
    UnsupportedManifestVersion(u32),
    #[error("checkpoint requires a supervisor opened with durable storage")]
    DurabilityRequired,
    #[error("{operation}: {message}")]
    Backend {
        operation: &'static str,
        message: String,
    },
}

fn publish_manifest(storage: &Path, manifest: &ForestManifest) -> Result<(), SupervisorError> {
    let mut temporary = tempfile::NamedTempFile::new_in(storage)?;
    serde_json::to_writer(&mut temporary, manifest)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(storage.join("forest.json"))
        .map_err(|error| error.error)?;
    File::open(storage)?.sync_all()?;
    Ok(())
}

impl SupervisorError {
    fn backend(operation: &'static str, error: impl std::error::Error) -> Self {
        Self::Backend {
            operation,
            message: error.to_string(),
        }
    }
}
