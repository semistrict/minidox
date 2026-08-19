# minidox

Shared Redox filesystem and page cache for microVMs over virtiofs DAX.

The page cache is extracted from RedoxFS's `Fmap` and `FileMmapInfo` design:
one page-aligned backing object per inode, shared mapping refcounts, versioned
invalidation, load-on-map, and writeback on unmap or sync. A host file replaces
the scheme process's anonymous mapping so a virtiofs device can install the same
pages into the DAX windows of multiple VMs.

## Workspace

- `crates/minidox-cache`: shared mmap/DAX cache and its transport-facing mapping
- `crates/minidox-redoxfs`: adapter to the RedoxFS transaction engine
- `vendor/redoxfs`: RedoxFS subtree at `7872ef8bc605e558da1255a9b0af3218cc993f58`
- `vendor/redox-kernel`: kernel mmap/scheme subtree at `d50854b68dcf04a8554ec08e94f2e23213aae5c1`

The vendored trees retain their upstream licenses and history metadata. Update
them with `git subtree pull` from full local clones of the canonical upstreams.
