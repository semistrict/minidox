//! Adapter between the extracted mmap cache and the vendored RedoxFS engine.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use minidox_cache::{NodeId, NodeStore};
use redoxfs::{Disk, FileSystem, Node, TreePtr};

pub struct RedoxNodeStore<D: Disk> {
    fs: FileSystem<D>,
}

impl<D: Disk> RedoxNodeStore<D> {
    pub fn new(fs: FileSystem<D>) -> Self {
        Self { fs }
    }

    pub fn filesystem(&self) -> &FileSystem<D> {
        &self.fs
    }

    pub fn filesystem_mut(&mut self) -> &mut FileSystem<D> {
        &mut self.fs
    }

    pub fn into_filesystem(self) -> FileSystem<D> {
        self.fs
    }
}

impl<D: Disk> NodeStore for RedoxNodeStore<D> {
    fn len(&mut self, node: NodeId) -> io::Result<u64> {
        self.fs
            .tx(|tx| {
                let node = tx.read_tree(TreePtr::<Node>::new(node))?;
                Ok(node.data().size())
            })
            .map_err(|error| io::Error::from_raw_os_error(error.errno))
    }

    fn read_node(&mut self, node: NodeId, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let now = now()?;
        self.fs
            .tx(|tx| {
                tx.read_node(
                    TreePtr::<Node>::new(node),
                    offset,
                    buf,
                    now.as_secs(),
                    now.subsec_nanos(),
                )
            })
            .map_err(|error| io::Error::from_raw_os_error(error.errno))
    }

    fn write_node(&mut self, node: NodeId, offset: u64, buf: &[u8]) -> io::Result<usize> {
        let now = now()?;
        self.fs
            .tx(|tx| {
                tx.write_node(
                    TreePtr::<Node>::new(node),
                    offset,
                    buf,
                    now.as_secs(),
                    now.subsec_nanos(),
                )
            })
            .map_err(|error| io::Error::from_raw_os_error(error.errno))
    }

    fn on_open_node(&mut self, node: NodeId) -> io::Result<()> {
        self.fs
            .tx(|tx| tx.on_open_node(TreePtr::<Node>::new(node)))
            .map_err(|error| io::Error::from_raw_os_error(error.errno))
    }

    fn on_close_node(&mut self, node: NodeId) -> io::Result<()> {
        self.fs
            .tx(|tx| tx.on_close_node(TreePtr::<Node>::new(node)))
            .map_err(|error| io::Error::from_raw_os_error(error.errno))
    }
}

fn now() -> io::Result<std::time::Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use minidox_cache::{MapFlags, PAGE_SIZE, PageCache};
    use redoxfs::DiskMemory;
    use std::os::unix::fs::FileExt;

    #[test]
    fn dax_write_is_committed_through_redoxfs() {
        let disk = DiskMemory::new(16 * 1024 * 1024);
        let mut fs = FileSystem::create(disk, None, 1, 0).unwrap();
        let node = fs
            .tx(|tx| {
                let node = tx.create_node(
                    TreePtr::<Node>::root(),
                    "shared",
                    Node::MODE_FILE | 0o644,
                    1,
                    0,
                )?;
                tx.truncate_node(node.ptr(), PAGE_SIZE, 1, 0)?;
                Ok(node.ptr().id())
            })
            .unwrap();

        let mut cache = PageCache::new(RedoxNodeStore::new(fs));
        cache.open(node).unwrap();
        let mapping = cache
            .map(node, 0, PAGE_SIZE, MapFlags::READ | MapFlags::WRITE)
            .unwrap();
        mapping.backing().write_at(b"from dax", 128).unwrap();
        cache.unmap(mapping).unwrap();

        let mut observed = [0; 8];
        cache
            .store_mut()
            .read_node(node, 128, &mut observed)
            .unwrap();
        assert_eq!(&observed, b"from dax");
        cache.close(node).unwrap();
    }
}
