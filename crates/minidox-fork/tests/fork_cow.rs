use minidox_fork::{ForkForest, PAGE_SIZE};

#[test]
fn multiple_vms_reading_a_base_file_do_not_duplicate_page_cache() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest
        .write_file(source, "/shared/data", 32, b"from the base VM")
        .unwrap();
    let first_child = forest.fork_vm(source).unwrap();
    let second_child = forest.fork_vm(source).unwrap();
    let before_reads = forest.page_accounting().filesystem;

    assert_eq!(
        forest.read_file(source, "/shared/data", 32, 16).unwrap(),
        b"from the base VM"
    );
    assert_eq!(
        forest
            .read_file(first_child, "/shared/data", 32, 16)
            .unwrap(),
        b"from the base VM"
    );
    assert_eq!(
        forest
            .read_file(second_child, "/shared/data", 32, 16)
            .unwrap(),
        b"from the base VM"
    );

    let after_reads = forest.page_accounting().filesystem;
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(after_reads, before_reads);
}

#[test]
fn multiple_vms_reading_base_memory_do_not_duplicate_resident_pages() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 96, b"shared base RAM").unwrap();
    let first_child = forest.fork_vm(source).unwrap();
    let second_child = forest.fork_vm(source).unwrap();
    let before_reads = forest.page_accounting().memory;

    assert_eq!(
        forest.read_memory(source, 96, 15).unwrap(),
        b"shared base RAM"
    );
    assert_eq!(
        forest.read_memory(first_child, 96, 15).unwrap(),
        b"shared base RAM"
    );
    assert_eq!(
        forest.read_memory(second_child, 96, 15).unwrap(),
        b"shared base RAM"
    );

    let after_reads = forest.page_accounting().memory;
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(after_reads, before_reads);
}

#[test]
fn filesystem_writes_diverge_only_the_written_page() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest
        .write_file(source, "/shared/data", 0, b"base page")
        .unwrap();
    forest
        .write_file(source, "/shared/data", PAGE_SIZE as u64, b"unchanged page")
        .unwrap();
    let child = forest.fork_vm(source).unwrap();

    forest
        .write_file(child, "/shared/data", 0, b"child page")
        .unwrap();

    assert_eq!(
        forest.read_file(source, "/shared/data", 0, 9).unwrap(),
        b"base page"
    );
    assert_eq!(
        forest.read_file(child, "/shared/data", 0, 10).unwrap(),
        b"child page"
    );
    assert_eq!(
        forest
            .read_file(child, "/shared/data", PAGE_SIZE as u64, 14)
            .unwrap(),
        b"unchanged page"
    );

    let pages = forest.page_accounting().filesystem;
    assert_eq!(pages.resident_pages, 3);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn memory_writes_diverge_only_the_written_page() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 0, b"base RAM").unwrap();
    forest
        .write_memory(source, PAGE_SIZE as u64, b"unchanged RAM")
        .unwrap();
    let child = forest.fork_vm(source).unwrap();

    forest.write_memory(child, 0, b"child RAM").unwrap();

    assert_eq!(forest.read_memory(source, 0, 8).unwrap(), b"base RAM");
    assert_eq!(forest.read_memory(child, 0, 9).unwrap(), b"child RAM");
    assert_eq!(
        forest.read_memory(child, PAGE_SIZE as u64, 13).unwrap(),
        b"unchanged RAM"
    );

    let pages = forest.page_accounting().memory;
    assert_eq!(pages.resident_pages, 3);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn fork_shares_memory_and_filesystem_pages_without_copying_them() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 0, b"RAM page zero").unwrap();
    forest
        .write_memory(source, PAGE_SIZE as u64, b"RAM page one")
        .unwrap();
    forest
        .write_file(source, "/state", 0, b"file page zero")
        .unwrap();
    forest
        .write_file(source, "/state", PAGE_SIZE as u64, b"file page one")
        .unwrap();
    let before = forest.page_accounting();

    let _child = forest.fork_vm(source).unwrap();

    let after = forest.page_accounting();
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
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 128, b"before fork").unwrap();
    forest
        .write_file(source, "/state", 128, b"before fork")
        .unwrap();

    let child = forest.fork_vm(source).unwrap();
    forest.write_memory(source, 128, b"after fork!").unwrap();
    forest
        .write_file(source, "/state", 128, b"after fork!")
        .unwrap();

    assert_eq!(forest.read_memory(child, 128, 11).unwrap(), b"before fork");
    assert_eq!(
        forest.read_file(child, "/state", 128, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(forest.read_memory(source, 128, 11).unwrap(), b"after fork!");
    assert_eq!(
        forest.read_file(source, "/state", 128, 11).unwrap(),
        b"after fork!"
    );
}

#[test]
fn a_child_can_be_forked_after_its_pages_diverge() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 0, b"source").unwrap();
    forest.write_file(source, "/state", 0, b"source").unwrap();
    let child = forest.fork_vm(source).unwrap();
    forest.write_memory(child, 0, b"child!").unwrap();
    forest.write_file(child, "/state", 0, b"child!").unwrap();

    let grandchild = forest.fork_vm(child).unwrap();
    forest.write_memory(child, 0, b"new kid").unwrap();
    forest.write_file(child, "/state", 0, b"new kid").unwrap();

    assert_eq!(forest.read_memory(source, 0, 6).unwrap(), b"source");
    assert_eq!(forest.read_file(source, "/state", 0, 6).unwrap(), b"source");
    assert_eq!(forest.read_memory(grandchild, 0, 6).unwrap(), b"child!");
    assert_eq!(
        forest.read_file(grandchild, "/state", 0, 6).unwrap(),
        b"child!"
    );
    assert_eq!(forest.read_memory(child, 0, 7).unwrap(), b"new kid");
    assert_eq!(forest.read_file(child, "/state", 0, 7).unwrap(), b"new kid");
}

#[test]
fn a_child_keeps_its_memory_and_filesystem_after_the_source_exits() {
    let mut forest = ForkForest::new();
    let source = forest.create_vm();
    forest.write_memory(source, 64, b"inherited RAM").unwrap();
    forest
        .write_file(source, "/state", 64, b"inherited file")
        .unwrap();
    let child = forest.fork_vm(source).unwrap();

    forest.remove_vm(source).unwrap();
    let grandchild = forest.fork_vm(child).unwrap();

    assert_eq!(
        forest.read_memory(grandchild, 64, 13).unwrap(),
        b"inherited RAM"
    );
    assert_eq!(
        forest.read_file(grandchild, "/state", 64, 14).unwrap(),
        b"inherited file"
    );
}
