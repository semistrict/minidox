use minidox_fork::{PAGE_SIZE, Supervisor};

#[test]
fn sibling_forks_share_a_file_page_first_loaded_after_the_fork() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "cold-data", PAGE_SIZE as u64)
        .unwrap();
    let first_child = supervisor.fork_vm(source).unwrap();
    let second_child = supervisor.fork_vm(source).unwrap();

    assert_eq!(supervisor.page_accounting().filesystem.resident_pages, 0);
    assert_eq!(
        supervisor
            .read_file(first_child, file, 0, PAGE_SIZE)
            .unwrap(),
        vec![0; PAGE_SIZE]
    );
    assert_eq!(
        supervisor
            .read_file(second_child, file, 0, PAGE_SIZE)
            .unwrap(),
        vec![0; PAGE_SIZE]
    );

    let pages = supervisor.page_accounting().filesystem;
    assert_eq!(pages.resident_pages, 1);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn recursive_forks_share_a_cold_page_until_one_branch_writes_it() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "recursive-cold-data", PAGE_SIZE as u64)
        .unwrap();
    let child = supervisor.fork_vm(source).unwrap();
    let grandchild = supervisor.fork_vm(child).unwrap();
    let sibling = supervisor.fork_vm(source).unwrap();

    assert_eq!(
        supervisor.read_file(sibling, file, 0, PAGE_SIZE).unwrap(),
        vec![0; PAGE_SIZE]
    );
    assert_eq!(
        supervisor
            .read_file(grandchild, file, 0, PAGE_SIZE)
            .unwrap(),
        vec![0; PAGE_SIZE]
    );
    let cold = supervisor.page_accounting().filesystem;
    assert_eq!(cold.resident_pages, 1);
    assert_eq!(cold.shared_pages, 1);

    supervisor
        .write_file(child, file, 0, b"child page")
        .unwrap();

    assert_eq!(supervisor.read_file(source, file, 0, 10).unwrap(), [0; 10]);
    assert_eq!(supervisor.read_file(sibling, file, 0, 10).unwrap(), [0; 10]);
    assert_eq!(
        supervisor.read_file(grandchild, file, 0, 10).unwrap(),
        [0; 10]
    );
    assert_eq!(
        supervisor.read_file(child, file, 0, 10).unwrap(),
        b"child page"
    );
    let diverged = supervisor.page_accounting().filesystem;
    assert_eq!(diverged.resident_pages, 2);
    assert_eq!(diverged.shared_pages, 1);
}

#[test]
fn multiple_vms_reading_a_base_file_do_not_duplicate_page_cache() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "shared-data", PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_file(source, file, 32, b"from the base VM")
        .unwrap();
    let first_child = supervisor.fork_vm(source).unwrap();
    let second_child = supervisor.fork_vm(source).unwrap();
    let before_reads = supervisor.page_accounting().filesystem;

    assert_eq!(
        supervisor.read_file(source, file, 32, 16).unwrap(),
        b"from the base VM"
    );
    assert_eq!(
        supervisor.read_file(first_child, file, 32, 16).unwrap(),
        b"from the base VM"
    );
    assert_eq!(
        supervisor.read_file(second_child, file, 32, 16).unwrap(),
        b"from the base VM"
    );

    let after_reads = supervisor.page_accounting().filesystem;
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(after_reads, before_reads);
}

#[test]
fn multiple_vms_reading_base_memory_do_not_duplicate_resident_pages() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    supervisor
        .write_memory(source, 96, b"shared base RAM")
        .unwrap();
    let first_child = supervisor.fork_vm(source).unwrap();
    let second_child = supervisor.fork_vm(source).unwrap();
    let before_reads = supervisor.page_accounting().memory;

    assert_eq!(
        supervisor.read_memory(source, 96, 15).unwrap(),
        b"shared base RAM"
    );
    assert_eq!(
        supervisor.read_memory(first_child, 96, 15).unwrap(),
        b"shared base RAM"
    );
    assert_eq!(
        supervisor.read_memory(second_child, 96, 15).unwrap(),
        b"shared base RAM"
    );

    let after_reads = supervisor.page_accounting().memory;
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(after_reads, before_reads);
}

#[test]
fn filesystem_writes_diverge_only_the_written_page() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "shared-data", (PAGE_SIZE * 2) as u64)
        .unwrap();
    supervisor
        .write_file(source, file, 0, b"base page")
        .unwrap();
    supervisor
        .write_file(source, file, PAGE_SIZE as u64, b"unchanged page")
        .unwrap();
    let child = supervisor.fork_vm(source).unwrap();

    supervisor
        .write_file(child, file, 0, b"child page")
        .unwrap();

    assert_eq!(
        supervisor.read_file(source, file, 0, 9).unwrap(),
        b"base page"
    );
    assert_eq!(
        supervisor.read_file(child, file, 0, 10).unwrap(),
        b"child page"
    );
    assert_eq!(
        supervisor
            .read_file(child, file, PAGE_SIZE as u64, 14)
            .unwrap(),
        b"unchanged page"
    );

    let pages = supervisor.page_accounting().filesystem;
    assert_eq!(pages.resident_pages, 3);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn memory_writes_diverge_only_the_written_page() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    supervisor.write_memory(source, 0, b"base RAM").unwrap();
    supervisor
        .write_memory(source, PAGE_SIZE as u64, b"unchanged RAM")
        .unwrap();
    let child = supervisor.fork_vm(source).unwrap();

    supervisor.write_memory(child, 0, b"child RAM").unwrap();

    assert_eq!(supervisor.read_memory(source, 0, 8).unwrap(), b"base RAM");
    assert_eq!(supervisor.read_memory(child, 0, 9).unwrap(), b"child RAM");
    assert_eq!(
        supervisor.read_memory(child, PAGE_SIZE as u64, 13).unwrap(),
        b"unchanged RAM"
    );

    let pages = supervisor.page_accounting().memory;
    assert_eq!(pages.resident_pages, 3);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn fork_shares_memory_and_filesystem_pages_without_copying_them() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "state", (PAGE_SIZE * 2) as u64)
        .unwrap();
    supervisor
        .write_memory(source, 0, b"RAM page zero")
        .unwrap();
    supervisor
        .write_memory(source, PAGE_SIZE as u64, b"RAM page one")
        .unwrap();
    supervisor
        .write_file(source, file, 0, b"file page zero")
        .unwrap();
    supervisor
        .write_file(source, file, PAGE_SIZE as u64, b"file page one")
        .unwrap();
    let before = supervisor.page_accounting();

    let _child = supervisor.fork_vm(source).unwrap();

    let after = supervisor.page_accounting();
    assert_eq!(before.memory.resident_pages, 2);
    assert_eq!(before.filesystem.resident_pages, 2);
    assert_eq!(after.memory.resident_pages, before.memory.resident_pages);
    assert_eq!(
        after.filesystem.resident_pages,
        before.filesystem.resident_pages
    );
    assert_eq!(after.memory.shared_pages, 2);
    assert_eq!(after.filesystem.shared_pages, 2);
}

#[test]
fn fork_point_captures_memory_and_filesystem_together() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "state", PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_memory(source, 128, b"before fork")
        .unwrap();
    supervisor
        .write_file(source, file, 128, b"before fork")
        .unwrap();

    let child = supervisor.fork_vm(source).unwrap();
    supervisor
        .write_memory(source, 128, b"after fork!")
        .unwrap();
    supervisor
        .write_file(source, file, 128, b"after fork!")
        .unwrap();

    assert_eq!(
        supervisor.read_memory(child, 128, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(child, file, 128, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_memory(source, 128, 11).unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_file(source, file, 128, 11).unwrap(),
        b"after fork!"
    );
}

#[test]
fn a_child_can_be_forked_after_its_pages_diverge() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "state", PAGE_SIZE as u64)
        .unwrap();
    supervisor.write_memory(source, 0, b"source").unwrap();
    supervisor.write_file(source, file, 0, b"source").unwrap();
    let child = supervisor.fork_vm(source).unwrap();
    supervisor.write_memory(child, 0, b"child!").unwrap();
    supervisor.write_file(child, file, 0, b"child!").unwrap();

    let grandchild = supervisor.fork_vm(child).unwrap();
    supervisor.write_memory(child, 0, b"new kid").unwrap();
    supervisor.write_file(child, file, 0, b"new kid").unwrap();

    assert_eq!(supervisor.read_memory(source, 0, 6).unwrap(), b"source");
    assert_eq!(supervisor.read_file(source, file, 0, 6).unwrap(), b"source");
    assert_eq!(supervisor.read_memory(grandchild, 0, 6).unwrap(), b"child!");
    assert_eq!(
        supervisor.read_file(grandchild, file, 0, 6).unwrap(),
        b"child!"
    );
    assert_eq!(supervisor.read_memory(child, 0, 7).unwrap(), b"new kid");
    assert_eq!(supervisor.read_file(child, file, 0, 7).unwrap(), b"new kid");
}

#[test]
fn a_child_keeps_its_memory_and_filesystem_after_the_source_exits() {
    let mut supervisor = Supervisor::new();
    let source = supervisor.create_vm().unwrap();
    let file = supervisor
        .create_file(source, "state", PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_memory(source, 64, b"inherited RAM")
        .unwrap();
    supervisor
        .write_file(source, file, 64, b"inherited file")
        .unwrap();
    let child = supervisor.fork_vm(source).unwrap();

    supervisor.remove_vm(source).unwrap();
    let grandchild = supervisor.fork_vm(child).unwrap();

    assert_eq!(
        supervisor.read_memory(grandchild, 64, 13).unwrap(),
        b"inherited RAM"
    );
    assert_eq!(
        supervisor.read_file(grandchild, file, 64, 14).unwrap(),
        b"inherited file"
    );
}
