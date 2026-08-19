#!/bin/sh
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "run this setup script as root" >&2
    exit 1
fi

test_user=${1:-${SUDO_USER:-}}
if [ -z "$test_user" ] || ! id "$test_user" >/dev/null 2>&1; then
    echo "pass the non-root test user as the first argument" >&2
    exit 1
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y \
    build-essential \
    ca-certificates \
    cpio \
    curl \
    kmod \
    pkg-config \
    xz-utils \
    zstd

if ! getent group kvm >/dev/null 2>&1; then
    groupadd --system kvm
fi
usermod -aG kvm "$test_user"
if [ -e /dev/kvm ]; then
    chgrp kvm /dev/kvm
    chmod 0660 /dev/kvm
fi

test_home=$(getent passwd "$test_user" | cut -d: -f6)
if ! su - "$test_user" -c 'command -v cargo >/dev/null 2>&1'; then
    rustup_installer=/tmp/minidox-rustup-init.sh
    curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
        https://sh.rustup.rs -o "$rustup_installer"
    chmod 0755 "$rustup_installer"
    su - "$test_user" -c \
        "$rustup_installer -y --profile minimal --default-toolchain stable"
    rm -f "$rustup_installer"
fi

su - "$test_user" -c \
    "test -x '$test_home/.cargo/bin/cargo' && '$test_home/.cargo/bin/rustc' --version"
test -c /dev/kvm
grep -Eq '(^| )(vmx|svm)( |$)' /proc/cpuinfo
