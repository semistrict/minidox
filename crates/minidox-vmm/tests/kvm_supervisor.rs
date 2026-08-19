#![cfg(target_os = "linux")]

use std::path::Path;

use minidox_vmm::{KvmSupervisor, RAM_PAGE_SIZE};

#[test]
fn one_kvm_supervisor_fork_publishes_ram_redoxfs_and_dax() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM supervisor test: /dev/kvm is unavailable");
        return;
    }

    let mut supervisor = KvmSupervisor::new().unwrap();
    let source = supervisor.create_vm(RAM_PAGE_SIZE * 3).unwrap();
    let state = supervisor
        .create_file(source, "state", RAM_PAGE_SIZE as u64)
        .unwrap();
    let cold = supervisor
        .create_file(source, "cold", RAM_PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_memory(source, 128, b"before fork")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"before fork")
        .unwrap();

    let child = supervisor.fork_vm(source).unwrap();
    let shared = supervisor.page_accounting();
    assert_eq!(shared.memory.resident_pages, 3);
    assert_eq!(shared.memory.shared_pages, 3);
    assert_eq!(shared.filesystem.resident_pages, 1);
    assert_eq!(shared.filesystem.shared_pages, 1);

    assert_eq!(
        supervisor
            .read_file(source, cold, 0, RAM_PAGE_SIZE)
            .unwrap(),
        vec![0; RAM_PAGE_SIZE]
    );
    assert_eq!(
        supervisor.read_file(child, cold, 0, RAM_PAGE_SIZE).unwrap(),
        vec![0; RAM_PAGE_SIZE]
    );
    let cold_shared = supervisor.page_accounting().filesystem;
    assert_eq!(cold_shared.resident_pages, 2);
    assert_eq!(cold_shared.shared_pages, 2);

    supervisor
        .write_memory(source, 128, b"after fork!")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"after fork!")
        .unwrap();
    assert_eq!(
        supervisor.read_memory(child, 128, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(child, state, 128, 11).unwrap(),
        b"before fork"
    );

    let grandchild = supervisor.fork_vm(source).unwrap();
    supervisor.remove_vm(source).unwrap();
    assert_eq!(
        supervisor.read_memory(grandchild, 128, 11).unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_file(grandchild, state, 128, 11).unwrap(),
        b"after fork!"
    );
}
