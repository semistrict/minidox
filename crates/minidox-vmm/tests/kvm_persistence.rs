#![cfg(target_os = "linux")]

use minidox_vmm::{KvmSupervisor, RAM_PAGE_SIZE};
use std::path::Path;

#[test]
fn persisted_fork_forest_recovers_ram_filesystems_and_recursive_cow() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM persistence test: /dev/kvm is unavailable");
        return;
    }
    let storage = tempfile::tempdir().unwrap();
    let source;
    let child;
    let state;

    {
        let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
        source = supervisor.create_vm(3 * RAM_PAGE_SIZE).unwrap();
        state = supervisor
            .create_file(source, "state", RAM_PAGE_SIZE as u64)
            .unwrap();
        supervisor.write_memory(source, 128, b"shared ram").unwrap();
        supervisor
            .write_file(source, state, 128, b"shared file")
            .unwrap();

        child = supervisor.fork_vm(source).unwrap();
        supervisor
            .write_memory(source, RAM_PAGE_SIZE + 128, b"source ram")
            .unwrap();
        supervisor
            .write_file(source, state, 128, b"source file")
            .unwrap();
        supervisor
            .write_memory(child, 2 * RAM_PAGE_SIZE + 128, b"child ram")
            .unwrap();
        supervisor
            .write_file(child, state, 128, b"child file!")
            .unwrap();
        supervisor.checkpoint().unwrap();
    }

    let grandchild;
    {
        let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
        assert_eq!(supervisor.vm_ids(), vec![source, child]);
        assert_eq!(
            supervisor
                .read_memory(source, RAM_PAGE_SIZE + 128, 10)
                .unwrap(),
            b"source ram"
        );
        assert_eq!(
            supervisor
                .read_memory(source, 2 * RAM_PAGE_SIZE + 128, 9)
                .unwrap(),
            &[0; 9]
        );
        assert_eq!(
            supervisor
                .read_memory(child, RAM_PAGE_SIZE + 128, 10)
                .unwrap(),
            &[0; 10]
        );
        assert_eq!(
            supervisor
                .read_memory(child, 2 * RAM_PAGE_SIZE + 128, 9)
                .unwrap(),
            b"child ram"
        );
        assert_eq!(
            supervisor.read_file(source, state, 128, 11).unwrap(),
            b"source file"
        );
        assert_eq!(
            supervisor.read_file(child, state, 128, 11).unwrap(),
            b"child file!"
        );

        let accounting = supervisor.page_accounting();
        assert_eq!(accounting.memory.resident_pages, 5);
        assert_eq!(accounting.memory.shared_pages, 1);
        assert_eq!(accounting.filesystem.resident_pages, 2);

        grandchild = supervisor.fork_vm(child).unwrap();
        supervisor.write_memory(child, 128, b"child next").unwrap();
        supervisor
            .write_file(child, state, 128, b"child next!")
            .unwrap();
        supervisor.checkpoint().unwrap();
    }

    {
        let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
        assert_eq!(supervisor.vm_ids(), vec![source, child, grandchild]);
        assert_eq!(
            supervisor.read_memory(child, 128, 10).unwrap(),
            b"child next"
        );
        assert_eq!(
            supervisor.read_memory(grandchild, 128, 10).unwrap(),
            b"shared ram"
        );
        assert_eq!(
            supervisor.read_file(child, state, 128, 11).unwrap(),
            b"child next!"
        );
        assert_eq!(
            supervisor.read_file(grandchild, state, 128, 11).unwrap(),
            b"child file!"
        );
        supervisor.write_memory(child, 128, b"not saved!").unwrap();
        supervisor
            .write_file(child, state, 128, b"not saved!!")
            .unwrap();
    }

    {
        let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
        assert_eq!(
            supervisor.read_memory(child, 128, 10).unwrap(),
            b"child next"
        );
        assert_eq!(
            supervisor.read_file(child, state, 128, 11).unwrap(),
            b"child next!"
        );
    }
}
