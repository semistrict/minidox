# minidox

Shared Redox filesystem and page cache for microVMs over virtiofs DAX.

> [!WARNING]
> This is an experimental research prototype. It is not ready for production
> workloads, and its persistence and snapshot formats do not have compatibility
> guarantees yet.

The page cache is extracted from RedoxFS's `Fmap` and `FileMmapInfo` design.
One Supervisor-owned cache serves every VM in a filesystem lineage. Entries are
keyed by stable Redox file-object identity, page offset, and the page version
visible to a branch; unchanged forks therefore cold-fault one backing page,
while writes create page-granular CoW identities. A host file replaces the
scheme process's anonymous mapping so virtiofs can install the same page into
the DAX windows of multiple VMs.

## Workspace

- `crates/minidox-cache`: shared mmap/DAX cache and its transport-facing mapping
- `crates/minidox-redoxfs`: adapter to the RedoxFS transaction engine
- `crates/minidox-vmm`: embedded Cloud Hypervisor lifecycle, KVM RAM forks,
  and the in-process virtio-fs DAX backend
- `vendor/redoxfs`: RedoxFS subtree at `7872ef8bc605e558da1255a9b0af3218cc993f58`
- `vendor/redox-kernel`: kernel mmap/scheme subtree at `d50854b68dcf04a8554ec08e94f2e23213aae5c1`
- `vendor/cloud-hypervisor`: Cloud Hypervisor subtree used as the in-process VMM
- `vendor/libkrun-devices`: libkrun device subtree used as the virtio-fs/DAX reference

The vendored trees retain their upstream licenses and history metadata. Update
them with `git subtree pull` from full local clones of the canonical upstreams.

## Platform support

The complete VMM runs on Linux hosts with KVM. The platform-independent cache,
filesystem, and fork-model tests also run on macOS. Nested virtualization and
guest virtio-fs DAX support depend on the host and guest kernel configuration.

## Tests

Run the platform-independent suites with `cargo test --workspace --locked`.
On macOS, `scripts/test-lima.sh` stages a clean copy in a Lima VM, builds the
static guest verifier, runs the full Linux workspace and virtio-fs protocol
tests, and boots the nested-KVM fork test. The Lima VM must expose nested KVM
and contain a readable arm64 kernel at `/var/tmp/Image-arm64`; override the instance
and kernel path with `MINIDOX_LIMA_INSTANCE` and `MINIDOX_LIMA_KERNEL`. The
default kernel path is `/var/tmp/Image-arm64`; the runner includes the matching
installed virtio-fs module, overridable with `MINIDOX_LIMA_VIRTIOFS_MODULE`.
Set `MINIDOX_LIMA_GUEST_DAX=1` on a nested-KVM host that supports guest
`devm_memremap_pages()` for the DAX BAR; this additionally makes both forked
guests cold-mmap the same untouched file and asserts one shared cache page.

`python3 scripts/test-gce.py` creates a nested-virtualization GCE VM in
`us-east1-b`, prepares the fresh Debian host, builds the guest verifier from its
running kernel, runs the guest-DAX assertion, and deletes the VM even when setup
or testing fails. On an already-prepared native x86-64 Linux KVM host, pass
`--native` to run the same test without provisioning GCE.
Override its project, zone, machine type, or instance name with the corresponding
`MINIDOX_GCE_*` environment variable.

## License

The original minidox code is licensed under the Apache License 2.0. Vendored
code remains under its upstream license; see [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)
and the license files inside each `vendor/` subtree.
