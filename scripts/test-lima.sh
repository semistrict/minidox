#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
instance=${MINIDOX_LIMA_INSTANCE:-default}
kernel=${MINIDOX_LIMA_KERNEL:-/var/tmp/Image-arm64}
kernel_release=${MINIDOX_LIMA_KERNEL_RELEASE:-}
if [ -z "$kernel_release" ]; then
    kernel_release=$(limactl shell "$instance" -- strings "$kernel" |
        sed -n 's/^Linux version \([^ ]*\).*/\1/p' |
        head -n 1 |
        tr -d '\r')
fi
if [ -z "$kernel_release" ]; then
    kernel_release=$(limactl shell "$instance" -- uname -r | tr -d '\r')
fi
virtiofs_module=${MINIDOX_LIMA_VIRTIOFS_MODULE:-/lib/modules/$kernel_release/kernel/fs/fuse/virtiofs.ko.zst}
run_id="$(date +%s)-$$"
guest_work="/tmp/minidox-lima-test-$run_id"
guest_target="/var/tmp/minidox-lima-target-$run_id"
guest_fixture="/tmp/minidox-lima-initramfs-$run_id"
guest_initramfs="/tmp/minidox-lima-initramfs-$run_id.cpio"
guest_console="/tmp/minidox-lima-console-$run_id.log"

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if ! limactl shell "$instance" -- rm -rf -- \
        "$guest_work" "$guest_target" "$guest_fixture" \
        "$guest_initramfs" "$guest_console"; then
        status=1
    fi
    for path in "$guest_work" "$guest_target" "$guest_fixture" "$guest_initramfs" "$guest_console"; do
        if limactl shell "$instance" -- test -e "$path"; then
            echo "temporary Lima path still exists: $path" >&2
            status=1
        fi
    done
    exit "$status"
}
trap cleanup EXIT INT TERM

limactl shell "$instance" -- test -r "$kernel"
limactl shell "$instance" -- test -r "$virtiofs_module"
limactl shell "$instance" -- command -v cargo >/dev/null
limactl shell "$instance" -- command -v gcc >/dev/null
limactl shell "$instance" -- command -v cpio >/dev/null
limactl shell "$instance" -- mkdir -p "$guest_work" "$guest_fixture"

COPYFILE_DISABLE=1 tar --no-xattrs --exclude=.git --exclude=target -C "$repo_root" -cf - . |
    limactl shell "$instance" -- tar -C "$guest_work" -xf -

limactl shell "$instance" -- gcc -static -Os -s \
    -o "$guest_fixture/init" "$guest_work/tests/guest/virtiofs-dax-init.c"
case "$virtiofs_module" in
    *.zst)
        limactl shell "$instance" -- zstd -d -f \
            -o "$guest_fixture/virtiofs.ko" "$virtiofs_module"
        ;;
    *)
        limactl shell "$instance" -- cp "$virtiofs_module" "$guest_fixture/virtiofs.ko"
        ;;
esac
printf 'init\nvirtiofs.ko\n' |
    limactl shell --workdir="$guest_fixture" "$instance" -- \
        cpio -o -H newc -F "$guest_initramfs"

limactl shell --workdir="$guest_work" "$instance" -- \
    env CARGO_TARGET_DIR="$guest_target" cargo test --workspace --locked
limactl shell --workdir="$guest_work/vendor/cloud-hypervisor" "$instance" -- \
    env CARGO_TARGET_DIR="$guest_target" cargo test -p virtio-devices \
        in_process_fs --features kvm --locked
if [ "${MINIDOX_LIMA_GUEST_DAX:-0}" = 1 ]; then
    limactl shell --workdir="$guest_work" "$instance" -- \
        env CARGO_TARGET_DIR="$guest_target" \
            MINIDOX_TEST_KERNEL="$kernel" \
            MINIDOX_TEST_INITRAMFS="$guest_initramfs" \
            MINIDOX_TEST_CONSOLE="$guest_console" \
            MINIDOX_TEST_VIRTIOFS=1 \
            cargo test -p minidox-vmm --test cloud_hypervisor_fork_state \
            --locked -- --nocapture
else
    limactl shell --workdir="$guest_work" "$instance" -- \
        env CARGO_TARGET_DIR="$guest_target" \
            MINIDOX_TEST_KERNEL="$kernel" \
            MINIDOX_TEST_INITRAMFS="$guest_initramfs" \
            MINIDOX_TEST_CONSOLE="$guest_console" \
            cargo test -p minidox-vmm --test cloud_hypervisor_fork_state \
            --locked -- --nocapture
fi
