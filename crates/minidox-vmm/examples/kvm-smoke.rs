#[cfg(target_os = "linux")]
use minidox_vmm::CloudHypervisorVm;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let first = CloudHypervisorVm::start()?;
    let second = CloudHypervisorVm::start()?;

    first.shutdown()?;
    second.shutdown()?;

    println!("started and stopped two in-process KVM VMM workers");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the KVM smoke probe must run on Linux");
}
