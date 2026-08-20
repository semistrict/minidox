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

#[test]
fn branch_exposes_root_directory_metadata_for_virtiofs() {
    let mut branch = RedoxBranch::create(16 * 1024 * 1024).unwrap();
    let node = branch.create_file("guest-visible", 4096).unwrap();

    assert_eq!(branch.lookup(1, "guest-visible").unwrap(), node);
    assert_eq!(branch.metadata(node).unwrap().size, 4096);
    assert!(
        branch
            .entries(1)
            .unwrap()
            .iter()
            .any(|entry| entry.id == node && entry.name == "guest-visible")
    );
}

#[test]
fn durable_redox_disk_layers_restore_recursive_cow_sharing() {
    let storage = tempfile::tempdir().unwrap();
    let node;
    let source_state;
    let child_state;
    {
        let mut source = RedoxBranch::create_durable(storage.path(), 32 * 1024 * 1024).unwrap();
        node = source.create_file("state", 8192).unwrap();
        source.write(node, 128, b"shared").unwrap();

        let mut child = source.fork().unwrap();
        source.write(node, 4096, b"source").unwrap();
        child.write(node, 4096, b"child!").unwrap();
        source_state = source.durable_state().unwrap();
        child_state = child.durable_state().unwrap();
        source.write(node, 128, b"not saved").unwrap();
        child.write(node, 128, b"not saved").unwrap();
    }

    let mut restored =
        RedoxBranch::restore_lineage(storage.path(), vec![source_state, child_state]).unwrap();
    let mut child = restored.pop().unwrap();
    let mut source = restored.pop().unwrap();
    assert_eq!(source.read(node, 128, 6).unwrap(), b"shared");
    assert_eq!(child.read(node, 128, 6).unwrap(), b"shared");
    assert_eq!(source.read(node, 4096, 6).unwrap(), b"source");
    assert_eq!(child.read(node, 4096, 6).unwrap(), b"child!");
    assert!(RedoxBranch::block_accounting([&source, &child]).shared_blocks > 0);

    let mut grandchild = child.fork().unwrap();
    child.write(node, 128, b"next!!").unwrap();
    assert_eq!(child.read(node, 128, 6).unwrap(), b"next!!");
    assert_eq!(grandchild.read(node, 128, 6).unwrap(), b"shared");
}
