use std::os::unix::fs::FileExt;

use minidox_cache::{MapFlags, PAGE_SIZE, SharedPageCache};
use minidox_redoxfs::RedoxBranch;

#[test]
fn cold_nonzero_redoxfs_page_is_loaded_once_across_forks() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem.create_file("cold-shared", PAGE_SIZE).unwrap();
    filesystem.write(node, 128, b"stored before fork").unwrap();
    let (cache, source) = SharedPageCache::new(filesystem);
    let first_child = cache.fork(source).unwrap();
    let second_child = cache.fork(source).unwrap();
    cache.open(first_child, node).unwrap();
    cache.open(second_child, node).unwrap();

    let first = cache
        .map_page(first_child, node, 0, MapFlags::READ)
        .unwrap();
    let second = cache
        .map_page(second_child, node, 0, MapFlags::READ)
        .unwrap();
    assert_eq!(first.page_id(), second.page_id());
    let mut observed = [0; 18];
    second.backing().read_at(&mut observed, 128).unwrap();
    assert_eq!(&observed, b"stored before fork");
    cache.unmap(first_child, first).unwrap();
    cache.unmap(second_child, second).unwrap();

    let pages = cache
        .page_accounting([source, first_child, second_child])
        .unwrap();
    assert_eq!(pages.resident_pages, 1);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn editing_one_page_preserves_cold_sharing_for_untouched_pages() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem
        .create_file("page-granular", PAGE_SIZE * 2)
        .unwrap();
    filesystem.write(node, 0, b"base first page").unwrap();
    filesystem
        .write(node, PAGE_SIZE, b"shared second page")
        .unwrap();
    let (cache, source) = SharedPageCache::new(filesystem);
    let child = cache.fork(source).unwrap();
    cache.open(source, node).unwrap();
    cache.open(child, node).unwrap();

    let changed = cache
        .map_page(child, node, 0, MapFlags::READ | MapFlags::WRITE)
        .unwrap();
    changed.backing().write_at(b"child first page", 0).unwrap();
    cache.unmap(child, changed).unwrap();
    cache.sync(child, node).unwrap();

    let source_untouched = cache
        .map_page(source, node, PAGE_SIZE, MapFlags::READ)
        .unwrap();
    let child_untouched = cache
        .map_page(child, node, PAGE_SIZE, MapFlags::READ)
        .unwrap();
    assert_eq!(source_untouched.page_id(), child_untouched.page_id());
    let mut observed = [0; 18];
    child_untouched.backing().read_at(&mut observed, 0).unwrap();
    assert_eq!(&observed, b"shared second page");

    let pages = cache.page_accounting([source, child]).unwrap();
    assert_eq!(pages.resident_pages, 3);
    assert_eq!(pages.shared_pages, 1);
}

#[test]
fn forked_redoxfs_branches_share_one_dax_page_until_a_write() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem.create_file("shared", PAGE_SIZE).unwrap();
    filesystem.write(node, 0, b"base page").unwrap();

    let (cache, source) = SharedPageCache::new(filesystem);
    cache.open(source, node).unwrap();
    let source_mapping = cache.map_page(source, node, 0, MapFlags::READ).unwrap();
    let source_page_id = source_mapping.page_id();
    cache.unmap(source, source_mapping).unwrap();

    let child = cache.fork(source).unwrap();
    cache.open(child, node).unwrap();
    let before_reads = cache.page_accounting([source, child]).unwrap();

    let source_read = cache.map_page(source, node, 0, MapFlags::READ).unwrap();
    let child_read = cache.map_page(child, node, 0, MapFlags::READ).unwrap();
    assert_eq!(source_read.page_id(), source_page_id);
    assert_eq!(child_read.page_id(), source_page_id);
    assert_eq!(before_reads.resident_pages, 1);
    assert_eq!(before_reads.shared_pages, 1);
    assert_eq!(
        cache.page_accounting([source, child]).unwrap(),
        before_reads
    );
    cache.unmap(source, source_read).unwrap();
    cache.unmap(child, child_read).unwrap();

    let child_write = cache
        .map_page(child, node, 0, MapFlags::READ | MapFlags::WRITE)
        .unwrap();
    child_write.backing().write_at(b"child page", 0).unwrap();
    cache.unmap(child, child_write).unwrap();

    let source_read = cache.map_page(source, node, 0, MapFlags::READ).unwrap();
    let child_read = cache.map_page(child, node, 0, MapFlags::READ).unwrap();
    let mut source_bytes = [0; 10];
    let mut child_bytes = [0; 10];
    source_read.backing().read_at(&mut source_bytes, 0).unwrap();
    child_read.backing().read_at(&mut child_bytes, 0).unwrap();
    assert_eq!(&source_bytes, b"base page\0");
    assert_eq!(&child_bytes, b"child page");

    let after_write = cache.page_accounting([source, child]).unwrap();
    assert_eq!(after_write.resident_pages, 2);
    assert_eq!(after_write.shared_pages, 0);
}

#[test]
fn dirty_dax_contents_are_part_of_the_filesystem_fork_point() {
    let mut filesystem = RedoxBranch::create(32 * 1024 * 1024).unwrap();
    let node = filesystem.create_file("state", PAGE_SIZE).unwrap();
    filesystem.write(node, 0, b"on disk!!").unwrap();
    let (cache, source) = SharedPageCache::new(filesystem);
    cache.open(source, node).unwrap();

    let mapping = cache
        .map_page(source, node, 0, MapFlags::READ | MapFlags::WRITE)
        .unwrap();
    mapping.backing().write_at(b"from dax!", 0).unwrap();
    cache.unmap(source, mapping).unwrap();

    let child = cache.fork(source).unwrap();
    cache.open(child, node).unwrap();
    cache.sync(source, node).unwrap();

    let child_mapping = cache.map_page(child, node, 0, MapFlags::READ).unwrap();
    let mut observed = [0; 9];
    child_mapping.backing().read_at(&mut observed, 0).unwrap();
    assert_eq!(&observed, b"from dax!");
}
