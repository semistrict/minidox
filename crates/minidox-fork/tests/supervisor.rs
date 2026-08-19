use minidox_fork::{PAGE_SIZE, Supervisor};

#[test]
fn one_supervisor_fork_captures_ram_redoxfs_and_dax_atomically() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let state = supervisor
        .create_file(source, "state", PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_memory(source, 128, b"before fork")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"before fork")
        .unwrap();

    let child = supervisor.fork_vm(source).unwrap();
    let shared = supervisor.page_accounting();
    assert_eq!(shared.memory.shared_pages, 1);
    assert_eq!(shared.filesystem.shared_pages, 1);

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
    assert_eq!(
        supervisor.read_memory(source, 128, 11).unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_file(source, state, 128, 11).unwrap(),
        b"after fork!"
    );
}

#[test]
fn supervisor_children_remain_forkable_after_source_exit() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let state = supervisor
        .create_file(source, "state", PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_memory(source, 64, b"inherited RAM")
        .unwrap();
    supervisor
        .write_file(source, state, 64, b"inherited file")
        .unwrap();
    let child = supervisor.fork_vm(source).unwrap();

    supervisor.remove_vm(source).unwrap();
    let grandchild = supervisor.fork_vm(child).unwrap();

    assert_eq!(
        supervisor.read_memory(grandchild, 64, 13).unwrap(),
        b"inherited RAM"
    );
    assert_eq!(
        supervisor.read_file(grandchild, state, 64, 14).unwrap(),
        b"inherited file"
    );
}
