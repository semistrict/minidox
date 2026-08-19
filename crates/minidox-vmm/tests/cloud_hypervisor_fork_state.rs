#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use minidox_vmm::{KvmSupervisor, RAM_PAGE_SIZE, VmConfig};
use serde_json::json;

const RAM_SIZE: usize = 128 << 20;

#[test]
fn supervisor_forks_running_vm_ram_redoxfs_and_machine_state() {
    let Ok(kernel) = env::var("MINIDOX_TEST_KERNEL") else {
        eprintln!("skipping nested lifecycle test: MINIDOX_TEST_KERNEL is unset");
        return;
    };
    let Ok(initramfs) = env::var("MINIDOX_TEST_INITRAMFS") else {
        eprintln!("skipping nested lifecycle test: MINIDOX_TEST_INITRAMFS is unset");
        return;
    };
    let console = PathBuf::from(format!(
        "/tmp/minidox-fork-state-console-{}.log",
        std::process::id()
    ));
    let config: VmConfig = serde_json::from_value(json!({
        "payload": {
            "kernel": kernel,
            "initramfs": initramfs,
            "cmdline": "console=ttyAMA0 earlycon=pl011,0x09000000 panic=-1"
        },
        "memory": { "size": RAM_SIZE },
        "console": { "mode": "Null" },
        "serial": { "mode": "File", "file": console.to_string_lossy() }
    }))
    .unwrap();

    let mut supervisor = KvmSupervisor::new().unwrap();
    let source = supervisor.create_cloud_vm(config).unwrap();
    thread::sleep(Duration::from_secs(2));
    assert!(
        fs::read_to_string(&console)
            .unwrap()
            .contains("Linux version")
    );

    let state = supervisor
        .create_file(source, "state", RAM_PAGE_SIZE as u64)
        .unwrap();
    let memory_offset = RAM_SIZE - RAM_PAGE_SIZE;
    supervisor
        .write_memory(source, memory_offset, b"before fork")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"before fork")
        .unwrap();

    let child = supervisor.fork_vm(source).unwrap();
    let shared = supervisor.page_accounting();
    assert_eq!(shared.memory.resident_pages, RAM_SIZE / RAM_PAGE_SIZE);
    assert_eq!(shared.memory.shared_pages, RAM_SIZE / RAM_PAGE_SIZE);
    assert_eq!(shared.filesystem.resident_pages, 1);
    assert_eq!(shared.filesystem.shared_pages, 1);

    supervisor
        .write_memory(source, memory_offset, b"after fork!")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"after fork!")
        .unwrap();
    assert_eq!(
        supervisor.read_memory(child, memory_offset, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(child, state, 128, 11).unwrap(),
        b"before fork"
    );

    let grandchild = supervisor.fork_vm(source).unwrap();
    supervisor.remove_vm(source).unwrap();
    assert_eq!(
        supervisor
            .read_memory(grandchild, memory_offset, 11)
            .unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_file(grandchild, state, 128, 11).unwrap(),
        b"after fork!"
    );

    supervisor.remove_vm(child).unwrap();
    supervisor.remove_vm(grandchild).unwrap();
    fs::remove_file(console).unwrap();
}
