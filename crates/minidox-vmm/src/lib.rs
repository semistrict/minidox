//! In-process VMM boundary for minidox.
//!
//! Cloud Hypervisor supplies machine construction, KVM state, device pause,
//! and migration state. Minidox owns supervision, RAM generations, the atomic
//! RAM/filesystem fork barrier, and the virtio-fs DAX device.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod ram;

#[cfg(target_os = "linux")]
pub use linux::{CloudHypervisorVm, VmConfig};
#[cfg(target_os = "linux")]
pub use ram::{KvmGuestRam, RAM_PAGE_SIZE, RamAccounting};

/// Whether this build can instantiate the KVM-backed VMM.
pub const fn is_supported_host() -> bool {
    cfg!(target_os = "linux")
}

/// Failure at the minidox-to-VMM boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A Cloud Hypervisor or host primitive failed.
    #[error("{operation}: {message}")]
    Backend {
        operation: &'static str,
        message: String,
    },

    /// The VMM worker panicked.
    #[error("Cloud Hypervisor VMM worker panicked")]
    WorkerPanicked,

    /// Guest RAM must consist of one or more complete base pages.
    #[error("invalid guest RAM size {0}")]
    InvalidRamSize(usize),

    /// A host RAM access exceeded the guest memory slot.
    #[error("guest RAM range offset={offset} length={len} is out of bounds")]
    RamRange { offset: usize, len: usize },
}

impl Error {
    #[cfg(target_os = "linux")]
    fn backend(operation: &'static str, error: impl std::error::Error) -> Self {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }

        Self::Backend { operation, message }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_support_matches_linux_kvm_requirement() {
        assert_eq!(super::is_supported_host(), cfg!(target_os = "linux"));
    }
}
