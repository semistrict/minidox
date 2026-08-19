#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::env;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    use minidox_vmm::{CloudHypervisorVm, VmConfig};
    use serde_json::json;

    let mut args = env::args_os().skip(1);
    let kernel = args
        .next()
        .ok_or("missing kernel path")?
        .to_string_lossy()
        .into_owned();
    let initramfs = args
        .next()
        .ok_or("missing initramfs path")?
        .to_string_lossy()
        .into_owned();
    let console = args
        .next()
        .ok_or("missing console output path")?
        .to_string_lossy()
        .into_owned();
    if args.next().is_some() {
        return Err("expected: nested-boot KERNEL INITRAMFS CONSOLE".into());
    }

    let config: VmConfig = serde_json::from_value(json!({
        "payload": {
            "kernel": kernel,
            "initramfs": initramfs,
            "cmdline": "console=ttyAMA0 earlycon=pl011,0x09000000 panic=-1"
        },
        "console": {
            "mode": "Null"
        },
        "serial": {
            "mode": "File",
            "file": console
        }
    }))?;

    let vm = CloudHypervisorVm::start()?;
    vm.create(config)?;
    vm.boot()?;
    thread::sleep(Duration::from_secs(3));
    vm.pause()?;
    vm.resume()?;
    thread::sleep(Duration::from_secs(1));
    vm.shutdown()?;

    let output = fs::read_to_string(console)?;
    if !output.contains("Linux version") {
        return Err("nested guest did not reach the Linux kernel banner".into());
    }

    println!("booted, paused, resumed, and stopped a nested Linux VM");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the nested boot probe must run on Linux");
}
