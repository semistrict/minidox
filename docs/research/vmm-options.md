# VMM options for minidox

Research date: 2026-08-19. Sources are upstream documentation, specifications,
and source repositories. Links to moving `main` branches describe the state
observed on this date; any code imported with `git subtree` should be pinned to
an exact commit.

## Decision context

The target is a Linux-hosted supervisor that embeds the VMM and minidox in one
process, runs several stock-Linux guests, and gives each guest an in-process
virtio-fs device with DAX. Any running VM can be forked recursively. Guest RAM
and filesystem state must cross one atomic fork point, descendants must outlive
their sources, and the pause must remain below 10 ms at p99 without copying RAM
or filesystem data in the pause.

## Short answer

Use **Cloud Hypervisor's library crates as the primary VMM substrate**, but do
not adopt its current virtio-fs path. Replace the socket-backed vhost-user-fs
frontend with an in-process minidox virtio-fs/DAX device. Import the useful
in-process DAX transport and FUSE device code from **libkrun** rather than
inventing that layer from scratch.

This is not a claim that Cloud Hypervisor already implements the required VM
fork. It does not. It is the best starting point because it already has the
hard-to-reconstruct VM state machinery: synchronous VM/device pause, device
snapshot traits, dirty logging, restore, and migration. The live recursive CoW
RAM generation model and the atomic RAM/filesystem fork barrier are minidox
features regardless of the selected VMM.

## Verified comparison

| Candidate | Library embedding | Several VMs in one process | virtio-fs / DAX | Snapshot and fork support | License | Integration fit |
|---|---|---|---|---|---|---|
| Cloud Hypervisor crates | The repository contains a public Rust `vmm` library crate and a public `start_vmm_thread` API. [crate manifest](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/Cargo.toml), [library API](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/lib.rs#L443-L562) | One upstream `Vmm` owns at most one `Vm`; creating a second returns `VmAlreadyCreated`. Running several `Vmm` instances is not an upstream product contract. [ownership](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/lib.rs#L608-L668), [create path](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/lib.rs#L2175-L2187) | Stock virtio-fs is supported through a socket-backed vhost-user frontend. Upstream documentation explicitly says DAX is unavailable. [configuration](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/vm_config.rs#L539-L561), [virtio-fs documentation](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/fs.md#dax-feature) | VM pause includes devices and the hypervisor VM; snapshot, restore, migration, and dirty logging exist. Snapshot/restore compatibility across releases is not guaranteed. There is no live CoW fork API. [pause and dirty log](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/vm.rs#L3017-L3238), [status guarantees](https://github.com/cloud-hypervisor/cloud-hypervisor#3-status) | Apache-2.0 and BSD-3-Clause on existing files; new contributions are Apache-2.0. [licensing](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/CONTRIBUTING.md) | **Best primary base.** High effort, but the missing work is concentrated in multi-instance supervision, a custom RAM backend, and the in-process fs device. |
| libkrun | It is explicitly distributed as a dynamic library with a C API. Its current `main` is the unstable 2.0 line; upstream directs production users to `stable-*`. [project status](https://github.com/libkrun/libkrun#libkrun) | The API can allocate multiple configuration contexts, but the run call consumes a context, takes over its caller, and calls `exit()` when the VM stops. That lifecycle is incompatible with the supervisor without changing internals. [contexts](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h#L47-L63), [run semantics](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h#L1228-L1251) | It has an in-process virtio-fs implementation, and `krun_add_virtiofs2/3` explicitly configures a DAX shared-memory window. [DAX API](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h#L199-L248), [device SHM region](https://github.com/libkrun/libkrun/blob/main/src/devices/src/virtio/fs/device.rs#L42-L110) | The public pause/resume API is macOS/HVF-only and returns `ENOTSUP` on Linux. The current public header has no snapshot or restore API. [pause limitation](https://github.com/libkrun/libkrun/blob/main/include/libkrun.h#L1252-L1285) | Apache-2.0. [license](https://github.com/libkrun/libkrun/blob/main/LICENSE) | **Best DAX code donor; second-best primary base.** Starting here saves fs/DAX work but requires a new Linux lifecycle, pause, device-state snapshot, and RAM-generation layer. |
| rust-vmm building blocks | rust-vmm is intentionally a collection of reusable VMM components, not a complete supported VMM. Its former reference VMM was experimental, explicitly not production-ready, and is archived. [project model](https://github.com/rust-vmm/community#what-is-rust-vmm), [reference VMM](https://github.com/rust-vmm/vmm-reference) | Entirely up to the embedding application. This is flexibility, not supplied multi-VM orchestration. | The current `vm-virtio` device workspace lists block, console, queue, and vsock components but no virtio-fs device. [device workspace](https://github.com/rust-vmm/vm-virtio) | Low-level KVM, memory, queue, and event components exist, but no complete cross-device VM snapshot or recursive live-fork facility is supplied. | The normal project convention is Apache-2.0 OR BSD-3-Clause. [community requirements](https://github.com/rust-vmm/community#publishing-on-cratesio---requirements-list) | **Cleanest custom design, largest initial build.** Suitable if maintaining a tailored VMM becomes preferable to adapting a complete one. |
| crosvm | The tree is split into many Rust crates, but the supported architecture is a VMM program whose normal Linux design forks a sandboxed process per virtual device. It is not presented as a stable embedding SDK. [architecture](https://crosvm.dev/book/architecture/overview.html), [workspace](https://github.com/google/crosvm/blob/main/Cargo.toml) | No supported multi-VM-in-one-process mode is documented. Its default process-per-device design works against the accepted supervisor topology. | Stock virtio-fs is supported. The official fs documentation does not expose or claim a virtio-fs DAX mode; the separately documented DAX path is an experimental, read-only virtio-pmem ext2 device. [virtio-fs](https://crosvm.dev/book/devices/fs.html), [pmem DAX](https://crosvm.dev/book/devices/pmem/pmem_ext2.html) | Upstream calls snapshotting “highly experimental,” “100% not supported,” and limited to a small device set. It freezes vCPUs and device backends before serializing them. [snapshot status](https://crosvm.dev/book/architecture/snapshotting.html) | BSD-3-Clause. [license](https://github.com/google/crosvm/blob/main/LICENSE) | **Useful device/source reference, poor primary fit.** Adapting it means undoing process topology while also adding DAX and production snapshot semantics. |
| Firecracker | Firecracker contains a Rust `vmm` library crate, but that crate documents itself as running a single microVM and the product architecture is one VMM process per microVM. [VMM crate](https://github.com/firecracker-microvm/firecracker/blob/main/src/vmm/src/lib.rs#L6-L9), [design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md) | Multiple VMs per process are not a supported model. | The current device API lists block, vhost-user-block, net, vsock, rng, pmem, and memory devices, but no virtio-fs device. Its pmem device supports DAX but is a block device, not stock virtio-fs. [device matrix](https://github.com/firecracker-microvm/firecracker/blob/main/docs/device-api.md), [pmem DAX](https://github.com/firecracker-microvm/firecracker/blob/main/docs/pmem.md) | Snapshot/restore is mature enough to clone from a memory file using `MAP_PRIVATE`, which shares clean pages and CoWs later writes. Creating a full snapshot still writes a full guest-memory file while the VM is paused; diff snapshots are developer preview and are layers to merge, not a constant-time recursive live fork. Disks are managed separately. [snapshot design](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md#overview), [snapshot representation](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/versioning.md#overview) | Apache-2.0, with BSD-3-Clause portions inherited from crosvm. [FAQ](https://github.com/firecracker-microvm/firecracker/blob/main/FAQ.md#what-is-the-open-source-license-for-firecracker) | **Strong snapshot reference, weak base for this topology.** It requires both a new virtio-fs/DAX device and replacement of the one-process-per-VM assumptions. |

## What the candidates do not provide

The following are architectural inferences from the verified interfaces above,
not upstream guarantees:

1. **A saved snapshot is not the required fork primitive.** Firecracker and
   Cloud Hypervisor can serialize a paused VM and restore it, but the minidox
   fork pause may not write RAM-sized or dirty-page-sized data. Publishing the
   child must be metadata work; page materialization and persistence must occur
   after resume.
2. **`MAP_PRIVATE` solves only descendants restored from an already materialized
   memory file.** After such a child dirties anonymous CoW pages, recursively
   forking that live child needs a generation-aware memory backend. None of the
   compared VMMs exposes that abstraction.
3. **Several library instances probably can coexist, but that is not enough.**
   Cloud Hypervisor's types are the least hostile to several instances, yet
   process-global signal, terminal, metrics, sandboxing, and shutdown behavior
   must be audited and removed from the embedded path. This requires a
   multi-instance test before committing to a large subtree extraction.
4. **The DAX fork barrier must cover device state.** Pausing vCPUs alone is
   insufficient. Queue workers must stop accepting mutations, in-flight
   requests must reach a defined boundary, writable DAX mappings must be
   generation-sealed, and only then may the paired RAM and filesystem roots be
   published.

No source reviewed supplies the complete sub-10-ms recursive fork. The latency
target therefore needs an early KVM prototype before broad device integration.

## Ranked recommendation

1. **Cloud Hypervisor crates plus a minidox-native virtio-fs/DAX device.** Vendor
   a pinned subset containing the hypervisor abstraction, VM/vCPU state,
   architecture setup, memory manager, PCI virtio transport, migration traits,
   and only the required devices. Do not vendor its HTTP control plane or make
   vhost-user part of the core topology.
2. **libkrun internals as the primary base.** Choose this only if a short spike
   shows that adding Linux pause, returning lifecycle control to the caller,
   and serializing device/KVM state is smaller than replacing Cloud
   Hypervisor's fs frontend. In either path, libkrun is the preferred source for
   in-process virtio-fs DAX mechanics.
3. **A tailored VMM directly from rust-vmm crates.** This best matches the final
   ownership model, but it front-loads machine construction, interrupt/PCI
   transport, complete device state, and lifecycle work.
4. **Firecracker internals.** Its snapshot and `MAP_PRIVATE` clone machinery are
   valuable references, but the supported lifecycle and missing virtio-fs make
   it a larger structural change than Cloud Hypervisor.
5. **crosvm as the primary VMM.** Reuse specific virtio/FUSE ideas if useful,
   but do not adopt its process-per-device VMM architecture for this supervisor.

### First validation gate

Before importing a large VMM subtree, build one narrow prototype that creates
two KVM VMs in one process from one RAM generation, pauses one VM, publishes a
child generation without copying pages, resumes both, and demonstrates
branch-local writes followed by a recursive child fork. Measure only the pause
interval. If that cannot remain below 10 ms with representative dirty memory,
changing the surrounding VMM will not rescue the design.
