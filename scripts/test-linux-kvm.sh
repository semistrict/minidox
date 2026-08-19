#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
kernel_release=${MINIDOX_TEST_KERNEL_RELEASE:-$(uname -r)}
kernel=${MINIDOX_TEST_KERNEL:-/boot/vmlinuz-$kernel_release}
module_root=/lib/modules/$kernel_release
modprobe_bin=${MINIDOX_TEST_MODPROBE:-/sbin/modprobe}
virtiofs_module=${MINIDOX_TEST_VIRTIOFS_MODULE:-}
if [ -z "$virtiofs_module" ]; then
    virtiofs_module=$(find "$module_root" -type f \
        \( -name 'virtiofs.ko' -o -name 'virtiofs.ko.xz' -o -name 'virtiofs.ko.zst' \) \
        -print -quit)
fi
fuse_module=${MINIDOX_TEST_FUSE_MODULE:-}
if [ -z "$fuse_module" ]; then
    fuse_module=$(find "$module_root" -type f \
        \( -name 'fuse.ko' -o -name 'fuse.ko.xz' -o -name 'fuse.ko.zst' \) \
        -print -quit)
fi

test -c /dev/kvm
test -r "$kernel"
test -n "$virtiofs_module"
test -r "$virtiofs_module"
test -x "$modprobe_bin"

fixture=$(mktemp -d /tmp/minidox-gce-initramfs.XXXXXX)
initramfs=$(mktemp /tmp/minidox-gce-initramfs.XXXXXX.cpio)
console=$(mktemp /tmp/minidox-gce-console.XXXXXX.log)
target=$(mktemp -d /tmp/minidox-gce-target.XXXXXX)

cleanup() {
    status=$?
    trap - EXIT INT TERM
    if [ "$status" -ne 0 ]; then
        echo "guest console:" >&2
        tail -n 200 "$console" >&2 || true
    fi
    rm -rf -- "$fixture" "$target"
    rm -f -- "$initramfs" "$console"
    exit "$status"
}
trap cleanup EXIT INT TERM

unpack_module() {
    source=$1
    destination=$2
    case "$source" in
        *.xz) xz -d -c "$source" >"$destination" ;;
        *.zst) zstd -d -q -c "$source" >"$destination" ;;
        *) cp "$source" "$destination" ;;
    esac
}

gcc -static -Os -s -o "$fixture/init" \
    "$repo_root/tests/guest/virtiofs-dax-init.c"
module_manifest=$fixture/modules
: >"$module_manifest"
module_sources=$(
    for module in virtio_pci virtiofs; do
        $modprobe_bin -S "$kernel_release" --show-depends "$module"
    done | sed -n 's/^insmod \([^ ]*\).*/\1/p'
)
if [ -z "$module_sources" ]; then
    module_sources=$virtiofs_module
    if [ -n "$fuse_module" ]; then
        module_sources="$fuse_module $module_sources"
    fi
fi
module_index=0
for module_source in $module_sources; do
    module_name=module-$module_index.ko
    unpack_module "$module_source" "$fixture/$module_name"
    printf '/%s\n' "$module_name" >>"$module_manifest"
    module_index=$((module_index + 1))
done

(cd "$fixture" && find . -mindepth 1 -maxdepth 1 -printf '%P\n' | \
    cpio --quiet -o -H newc -F "$initramfs")

PATH="$HOME/.cargo/bin:$PATH" \
CARGO_TARGET_DIR="$target" \
MINIDOX_TEST_KERNEL="$kernel" \
MINIDOX_TEST_INITRAMFS="$initramfs" \
MINIDOX_TEST_CONSOLE="$console" \
MINIDOX_TEST_VIRTIOFS=1 \
cargo test --manifest-path "$repo_root/Cargo.toml" \
    -p minidox-vmm --test cloud_hypervisor_fork_state \
    --locked -- --nocapture
