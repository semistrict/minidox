use minidox_redoxfs::RedoxBranch;

#[test]
fn forked_redoxfs_branches_share_blocks_and_isolate_file_writes() {
    let mut source = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = source.create_file("state", 4096).unwrap();
    source.write(node, 128, b"before fork").unwrap();

    let mut child = source.fork().unwrap();
    let before_reads = RedoxBranch::block_accounting([&source, &child]);
    assert_eq!(source.read(node, 128, 11).unwrap(), b"before fork");
    assert_eq!(child.read(node, 128, 11).unwrap(), b"before fork");
    assert_eq!(
        RedoxBranch::block_accounting([&source, &child]),
        before_reads
    );
    assert!(before_reads.shared_blocks > 0);

    child.write(node, 128, b"child write").unwrap();

    assert_eq!(source.read(node, 128, 11).unwrap(), b"before fork");
    assert_eq!(child.read(node, 128, 11).unwrap(), b"child write");
}

#[test]
fn redoxfs_fork_does_not_copy_blocks_and_supports_recursive_forks() {
    let mut source = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = source.create_file("state", 8192).unwrap();
    source.write(node, 4096, b"source").unwrap();
    let before = RedoxBranch::block_accounting([&source]);

    let mut child = source.fork().unwrap();
    let after_first_fork = RedoxBranch::block_accounting([&source, &child]);
    assert_eq!(after_first_fork.resident_blocks, before.resident_blocks);
    assert_eq!(after_first_fork.shared_blocks, before.resident_blocks);

    child.write(node, 4096, b"child!").unwrap();
    let mut grandchild = child.fork().unwrap();
    child.write(node, 4096, b"new kid").unwrap();

    assert_eq!(source.read(node, 4096, 6).unwrap(), b"source");
    assert_eq!(grandchild.read(node, 4096, 6).unwrap(), b"child!");
    assert_eq!(child.read(node, 4096, 7).unwrap(), b"new kid");
}

#[test]
fn redoxfs_child_survives_after_source_is_dropped() {
    let mut source = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = source.create_file("state", 4096).unwrap();
    source.write(node, 64, b"inherited file").unwrap();
    let mut child = source.fork().unwrap();

    drop(source);
    let mut grandchild = child.fork().unwrap();

    assert_eq!(grandchild.read(node, 64, 14).unwrap(), b"inherited file");
}
