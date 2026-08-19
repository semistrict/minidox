# Cloud Hypervisor subtree

Cloud Hypervisor is imported at `vendor/cloud-hypervisor` with `git subtree`.
The current split commit is `75afe33d1685d8e1d205d92085011cb863be196f`.

`minidox-vmm` consumes the upstream `vmm` and `hypervisor` crates with KVM
enabled. It starts one VMM worker per `CloudHypervisorVm`, without starting the
upstream command-line binary, HTTP server, D-Bus server, or vhost-user-fs
backend. The minidox supervisor will supply its own control plane, RAM generation
manager, fork barrier, and in-process virtio-fs/DAX device.

Update the subtree from the upstream `main` branch with:

```sh
git subtree pull --prefix=vendor/cloud-hypervisor \
  https://github.com/cloud-hypervisor/cloud-hypervisor.git main --squash
```
