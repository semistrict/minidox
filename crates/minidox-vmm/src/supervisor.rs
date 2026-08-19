use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use cloud_hypervisor_hypervisor::{Hypervisor, HypervisorVmConfig, Vm};
use minidox_cache::{BranchingPageCache, CacheError, CachePageAccounting, NodeId};
use minidox_redoxfs::RedoxBranch;

use crate::{CloudHypervisorVm, KvmGuestRam, RamAccounting, VmConfig};

const RAM_SLOT: u32 = 0;
const RAM_GPA: u64 = 0;
const DEFAULT_FILESYSTEM_SIZE: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct KvmVmId(u64);

struct SupervisedVm {
    backend: VmBackend,
    ram: KvmGuestRam,
    filesystem: BranchingPageCache<RedoxBranch>,
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
    vms: BTreeMap<KvmVmId, SupervisedVm>,
}

impl KvmSupervisor {
    pub fn new() -> Result<Self, SupervisorError> {
        Ok(Self {
            hypervisor: cloud_hypervisor_hypervisor::new()
                .map_err(|error| SupervisorError::backend("create KVM hypervisor", error))?,
            next_vm: 1,
            vms: BTreeMap::new(),
        })
    }

    pub fn create_vm(&mut self, ram_size: usize) -> Result<KvmVmId, SupervisorError> {
        let vm = self
            .hypervisor
            .create_vm(HypervisorVmConfig::default())
            .map_err(|error| SupervisorError::backend("create KVM VM", error))?;
        let ram = KvmGuestRam::new(ram_size)?;
        ram.register(vm.as_ref(), RAM_SLOT, RAM_GPA)?;
        let filesystem = match RedoxBranch::create(DEFAULT_FILESYSTEM_SIZE) {
            Ok(filesystem) => BranchingPageCache::new(filesystem),
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
    pub fn create_cloud_vm(&mut self, config: VmConfig) -> Result<KvmVmId, SupervisorError> {
        let ram_size = usize::try_from(config.memory.size)
            .map_err(|_| crate::Error::InvalidRamSize(usize::MAX))?;
        let ram = KvmGuestRam::new(ram_size)?;
        let filesystem = BranchingPageCache::new(RedoxBranch::create(DEFAULT_FILESYSTEM_SIZE)?);
        let backend = CloudHypervisorVm::start()?;
        backend.create(config)?;
        backend.boot_with_ram(&ram)?;
        backend.begin_fork_tracking()?;

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
            .store_mut()
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
        Ok(child)
    }

    fn fork_cloud_vm(&mut self, source: KvmVmId) -> Result<KvmVmId, SupervisorError> {
        let source_vm = self.vm_mut(source)?;
        let VmBackend::Cloud(source_backend) = &source_vm.backend else {
            unreachable!("backend kind was checked by fork_vm")
        };
        source_backend.pause()?;

        let fork_result = (|| {
            let capture = source_backend.capture_fork_state()?;
            let child_filesystem = source_vm.filesystem.fork()?;
            let child_ram = source_vm.ram.fork_dirty_pages(&capture.dirty_ram_pages)?;
            let child_backend = CloudHypervisorVm::start()?;
            child_backend.restore_fork_state(capture, &child_ram)?;
            child_backend.begin_fork_tracking()?;
            child_backend.resume()?;
            Ok::<_, SupervisorError>((child_backend, child_ram, child_filesystem))
        })();

        let resume_result = source_backend.resume().map_err(SupervisorError::from);
        let (child_backend, child_ram, child_filesystem) = fork_result?;
        if let Err(error) = resume_result {
            drop(child_backend);
            return Err(error);
        }

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
        Ok(child)
    }

    pub fn remove_vm(&mut self, vm: KvmVmId) -> Result<(), SupervisorError> {
        self.vms
            .remove(&vm)
            .map(drop)
            .ok_or(SupervisorError::VmNotFound(vm))
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
        let result: Result<(), SupervisorError> = (|| {
            let cache = &mut vm.filesystem;
            cache.open(node)?;
            let write_result = cache.write(node, offset, bytes);
            let close_result = cache.close(node);
            write_result?;
            close_result?;
            Ok(())
        })();
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
        let mut bytes = vec![0; len];
        let result: Result<usize, SupervisorError> = (|| {
            let cache = &mut vm.filesystem;
            cache.open(node)?;
            let read_result = cache.read(node, offset, &mut bytes);
            let close_result = cache.close(node);
            let read = read_result?;
            close_result?;
            Ok(read)
        })();
        let resume_result = vm.backend.resume();
        let read = result?;
        resume_result?;
        bytes.truncate(read);
        Ok(bytes)
    }

    pub fn page_accounting(&self) -> KvmForkAccounting {
        KvmForkAccounting {
            memory: KvmGuestRam::page_accounting(self.vms.values().map(|vm| &vm.ram)),
            filesystem: BranchingPageCache::page_accounting(
                self.vms.values().map(|vm| &vm.filesystem),
            ),
        }
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
    #[error("{operation}: {message}")]
    Backend {
        operation: &'static str,
        message: String,
    },
}

impl SupervisorError {
    fn backend(operation: &'static str, error: impl std::error::Error) -> Self {
        Self::Backend {
            operation,
            message: error.to_string(),
        }
    }
}
