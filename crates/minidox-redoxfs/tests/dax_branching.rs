use std::os::unix::fs::FileExt;

use minidox_cache::{BranchingPageCache, MapFlags, PAGE_SIZE};
use minidox_redoxfs::RedoxBranch;

#[test]
fn forked_redoxfs_branches_share_one_dax_page_until_a_write() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem.create_file("shared", PAGE_SIZE).unwrap();
    filesystem.write(node, 0, b"base page").unwrap();

    let mut source = BranchingPageCache::new(filesystem);
    source.open(node).unwrap();
    let source_mapping = source.map_page(node, 0, MapFlags::READ).unwrap();
    let source_page_id = source_mapping.page_id();
    source.unmap(source_mapping).unwrap();

    let mut child = source.fork().unwrap();
    child.open(node).unwrap();
    let before_reads = BranchingPageCache::page_accounting([&source, &child]);

    let source_read = source.map_page(node, 0, MapFlags::READ).unwrap();
    let child_read = child.map_page(node, 0, MapFlags::READ).unwrap();
    assert_eq!(source_read.page_id(), source_page_id);
    assert_eq!(child_read.page_id(), source_page_id);
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(
        BranchingPageCache::page_accounting([&source, &child]),
        before_reads
    );
    source.unmap(source_read).unwrap();
    child.unmap(child_read).unwrap();

    let child_write = child
        .map_page(node, 0, MapFlags::READ | MapFlags::WRITE)
        .unwrap();
    child_write.backing().write_at(b"child page", 0).unwrap();
    child.unmap(child_write).unwrap();

    let source_read = source.map_page(node, 0, MapFlags::READ).unwrap();
    let child_read = child.map_page(node, 0, MapFlags::READ).unwrap();
    let mut source_bytes = [0; 10];
    let mut child_bytes = [0; 10];
    source_read.backing().read_at(&mut source_bytes, 0).unwrap();
    child_read.backing().read_at(&mut child_bytes, 0).unwrap();
    assert_eq!(&source_bytes, b"base page\0");
    assert_eq!(&child_bytes, b"child page");

    let after_write = BranchingPageCache::page_accounting([&source, &child]);
    assert_eq!(after_write.resident_pages, 2);
    assert_eq!(after_write.shared_pages, 0);
}

#[test]
fn dirty_dax_contents_are_part_of_the_filesystem_fork_point() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem.create_file("state", PAGE_SIZE).unwrap();
    filesystem.write(node, 0, b"on disk!!").unwrap();
    let mut source = BranchingPageCache::new(filesystem);
    source.open(node).unwrap();

    let mapping = source
        .map_page(node, 0, MapFlags::READ | MapFlags::WRITE)
        .unwrap();
    mapping.backing().write_at(b"from dax!", 0).unwrap();
    source.unmap(mapping).unwrap();

    let mut child = source.fork().unwrap();
    child.open(node).unwrap();
    source.sync(node).unwrap();

    let child_mapping = child.map_page(node, 0, MapFlags::READ).unwrap();
    let mut observed = [0; 9];
    child_mapping.backing().read_at(&mut observed, 0).unwrap();
    assert_eq!(&observed, b"from dax!");
}
