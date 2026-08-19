use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use cloud_hypervisor::api::{
    ApiAction, ApiRequest, VmBoot, VmCreate, VmPause, VmResume, VmmShutdown,
};
pub use cloud_hypervisor::vm_config::VmConfig;
use seccompiler::SeccompAction;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::Error;

/// One Cloud Hypervisor VMM worker embedded in the minidox supervisor.
///
/// This deliberately does not start Cloud Hypervisor's HTTP or D-Bus control
/// planes. Each value owns one VM; a supervisor can own several values in the
/// same process.
pub struct CloudHypervisorVm {
    api_event: EventFd,
    api_sender: Sender<ApiRequest>,
    worker: Option<JoinHandle<cloud_hypervisor::Result<()>>>,
}

impl CloudHypervisorVm {
    /// Start an empty VMM worker backed by KVM.
    pub fn start() -> Result<Self, Error> {
        let (api_sender, api_receiver) = channel();
        let api_event = EventFd::new(EFD_NONBLOCK)
            .map_err(|error| Error::backend("create VMM API eventfd", error))?;
        let exit_event = EventFd::new(EFD_NONBLOCK)
            .map_err(|error| Error::backend("create VMM exit eventfd", error))?;
        let hypervisor = cloud_hypervisor_hypervisor::new()
            .map_err(|error| Error::backend("create KVM hypervisor", error))?;

        let handle = cloud_hypervisor::start_vmm_thread(
            cloud_hypervisor::VmmVersionInfo::new(
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_VERSION"),
            ),
            &None,
            None,
            api_event
                .try_clone()
                .map_err(|error| Error::backend("clone VMM API eventfd", error))?,
            api_sender.clone(),
            api_receiver,
            exit_event
                .try_clone()
                .map_err(|error| Error::backend("clone VMM exit eventfd", error))?,
            &SeccompAction::Allow,
            Arc::clone(&hypervisor),
            true,
            false,
        )
        .map_err(|error| Error::backend("start VMM worker", error))?;

        debug_assert!(handle.http_api_handle.is_none());

        Ok(Self {
            api_event,
            api_sender,
            worker: Some(handle.thread_handle),
        })
    }

    /// Install a Cloud Hypervisor VM configuration in this worker.
    pub fn create(&self, config: VmConfig) -> Result<(), Error> {
        self.send(&VmCreate, Box::new(config), "create VM")
    }

    /// Boot the configured VM.
    pub fn boot(&self) -> Result<(), Error> {
        self.send(&VmBoot, (), "boot VM").map(drop)
    }

    /// Pause vCPUs and devices at the VMM boundary.
    pub fn pause(&self) -> Result<(), Error> {
        self.send(&VmPause, (), "pause VM").map(drop)
    }

    /// Resume vCPUs and devices.
    pub fn resume(&self) -> Result<(), Error> {
        self.send(&VmResume, (), "resume VM").map(drop)
    }

    /// Stop the VMM worker and wait for it to exit.
    pub fn shutdown(mut self) -> Result<(), Error> {
        self.shutdown_inner()
    }

    fn send<Action: ApiAction>(
        &self,
        action: &Action,
        body: Action::RequestBody,
        operation: &'static str,
    ) -> Result<Action::ResponseBody, Error> {
        action
            .send(
                self.api_event
                    .try_clone()
                    .map_err(|error| Error::backend("clone VMM API eventfd", error))?,
                self.api_sender.clone(),
                body,
            )
            .map_err(|error| Error::backend(operation, error))
    }

    fn shutdown_inner(&mut self) -> Result<(), Error> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };

        let request_result = self.send(&VmmShutdown, (), "shut down VMM");
        let join_result = worker.join().map_err(|_| Error::WorkerPanicked)?;
        join_result.map_err(|error| Error::backend("VMM worker exit", error))?;
        request_result
    }
}

impl Drop for CloudHypervisorVm {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}
