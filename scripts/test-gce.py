#!/usr/bin/env python3
"""Provision and run the native-KVM guest-DAX test on GCE or locally."""

from __future__ import annotations

import argparse
import grp
import lzma
import os
import pwd
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
INSTANCE_RE = re.compile(r"[a-z](?:[-a-z0-9]{0,61}[a-z0-9])?")


def run(
    command: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    capture: bool = False,
    check: bool = True,
    stdin: bytes | None = None,
    stdout=None,
) -> subprocess.CompletedProcess[str] | subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=check,
        input=stdin,
        stdout=subprocess.PIPE if capture else stdout,
        stderr=subprocess.PIPE if capture else None,
        text=capture,
    )


def captured(command: list[str]) -> str:
    result = run(command, capture=True)
    assert isinstance(result.stdout, str)
    return result.stdout.strip()


def validate_instance_name(instance: str) -> None:
    if not INSTANCE_RE.fullmatch(instance):
        raise SystemExit(f"invalid GCE instance name: {instance!r}")


def source_archive(destination: Path) -> None:
    def filter_entry(entry: tarfile.TarInfo) -> tarfile.TarInfo | None:
        parts = Path(entry.name).parts
        if ".git" in parts or "target" in parts:
            return None
        return entry

    with tarfile.open(destination, "w:gz") as archive:
        archive.add(REPO_ROOT, arcname=".", filter=filter_entry)


def gcloud_base(project: str, zone: str) -> list[str]:
    return ["--project", project, "--zone", zone]


def delete_instance(instance: str, project: str, zone: str) -> None:
    location = gcloud_base(project, zone)
    run(
        ["gcloud", "compute", "instances", "delete", instance, *location, "--quiet"],
        check=False,
    )
    remaining = run(
        ["gcloud", "compute", "instances", "describe", instance, *location],
        capture=True,
        check=False,
    )
    if remaining.returncode == 0:
        raise RuntimeError(f"temporary GCE VM still exists: {instance}")


def wait_for_ssh(instance: str, project: str, zone: str) -> None:
    location = gcloud_base(project, zone)
    for _ in range(36):
        result = run(
            [
                "gcloud",
                "compute",
                "ssh",
                instance,
                *location,
                "--command=true",
                "--ssh-flag=-o ConnectTimeout=5",
            ],
            capture=True,
            check=False,
        )
        if result.returncode == 0:
            return
        time.sleep(5)
    raise RuntimeError("GCE VM did not become reachable over SSH")


def run_on_gce() -> None:
    project = os.environ.get("MINIDOX_GCE_PROJECT") or captured(
        ["gcloud", "config", "get-value", "project"]
    )
    if not project or project == "(unset)":
        raise SystemExit("set MINIDOX_GCE_PROJECT or configure a gcloud project")
    zone = os.environ.get("MINIDOX_GCE_ZONE", "us-east1-b")
    machine_type = os.environ.get("MINIDOX_GCE_MACHINE_TYPE", "n2-standard-4")
    instance = os.environ.get(
        "MINIDOX_GCE_INSTANCE",
        f"minidox-dax-{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}",
    )
    validate_instance_name(instance)
    remote_work = f"/var/tmp/{instance}"
    location = gcloud_base(project, zone)

    with tempfile.TemporaryDirectory(prefix="minidox-gce-") as temporary:
        archive = Path(temporary) / "source.tar.gz"
        source_archive(archive)
        existing = run(
            ["gcloud", "compute", "instances", "describe", instance, *location],
            capture=True,
            check=False,
        )
        if existing.returncode == 0:
            raise RuntimeError(f"refusing to replace existing GCE VM: {instance}")
        try:
            run(
                [
                    "gcloud",
                    "compute",
                    "instances",
                    "create",
                    instance,
                    *location,
                    f"--machine-type={machine_type}",
                    "--image-family=debian-12",
                    "--image-project=debian-cloud",
                    "--boot-disk-size=50GB",
                    "--boot-disk-type=pd-balanced",
                    "--enable-nested-virtualization",
                    "--max-run-duration=2h",
                    "--instance-termination-action=DELETE",
                    "--no-service-account",
                    "--no-scopes",
                ]
            )
            wait_for_ssh(instance, project, zone)
            run(
                [
                    "gcloud",
                    "compute",
                    "scp",
                    str(archive),
                    f"{instance}:/tmp/minidox-source.tar.gz",
                    *location,
                    "--quiet",
                ]
            )
            quoted_work = shlex.quote(remote_work)
            quoted_script = shlex.quote(f"{remote_work}/scripts/test-gce.py")
            setup = (
                f"mkdir -p {quoted_work} && "
                f"tar -C {quoted_work} -xzf /tmp/minidox-source.tar.gz && "
                f"sudo python3 {quoted_script} --setup-host \"$USER\""
            )
            run(
                [
                    "gcloud",
                    "compute",
                    "ssh",
                    instance,
                    *location,
                    f"--command={setup}",
                ]
            )
            native = f"cd {quoted_work} && python3 {quoted_script} --native"
            run(
                [
                    "gcloud",
                    "compute",
                    "ssh",
                    instance,
                    *location,
                    "--ssh-flag=-o ServerAliveInterval=30",
                    f"--command={native}",
                ]
            )
        finally:
            active_error = sys.exc_info()[1]
            try:
                delete_instance(instance, project, zone)
            except Exception as cleanup_error:
                if active_error is None:
                    raise
                print(f"GCE cleanup failed: {cleanup_error}", file=sys.stderr)


def setup_host(test_user: str) -> None:
    if os.geteuid() != 0:
        raise SystemExit("run --setup-host as root")
    try:
        account = pwd.getpwnam(test_user)
    except KeyError as error:
        raise SystemExit(f"unknown non-root test user: {test_user}") from error

    environment = dict(os.environ, DEBIAN_FRONTEND="noninteractive")
    run(["apt-get", "update"], env=environment)
    run(
        [
            "apt-get",
            "install",
            "-y",
            "build-essential",
            "ca-certificates",
            "cpio",
            "curl",
            "kmod",
            "pkg-config",
            "xz-utils",
            "zstd",
        ],
        env=environment,
    )

    try:
        grp.getgrnam("kvm")
    except KeyError:
        run(["groupadd", "--system", "kvm"])
    run(["usermod", "-aG", "kvm", test_user])
    kvm = Path("/dev/kvm")
    if kvm.exists():
        run(["chgrp", "kvm", str(kvm)])
        kvm.chmod(0o660)

    cargo_check = run(
        ["su", "-", test_user, "-c", "command -v cargo >/dev/null 2>&1"],
        check=False,
    )
    cargo = Path(account.pw_dir) / ".cargo/bin/cargo"
    rustc = Path(account.pw_dir) / ".cargo/bin/rustc"
    if cargo_check.returncode != 0:
        installer = Path("/tmp/minidox-rustup-init.sh")
        try:
            urllib.request.urlretrieve("https://sh.rustup.rs", installer)
            installer.chmod(0o755)
            command = (
                f"{shlex.quote(str(installer))} -y --profile minimal "
                "--default-toolchain stable"
            )
            run(["su", "-", test_user, "-c", command])
        finally:
            installer.unlink(missing_ok=True)
    if not cargo.is_file() or not os.access(cargo, os.X_OK):
        raise RuntimeError(f"cargo was not installed at {cargo}")
    run(["su", "-", test_user, "-c", f"{shlex.quote(str(rustc))} --version"])

    if not kvm.exists() or not stat.S_ISCHR(kvm.stat().st_mode):
        raise RuntimeError("/dev/kvm is not a character device")
    cpuinfo = Path("/proc/cpuinfo").read_text()
    if not re.search(r"(?:^|[ ])(?:vmx|svm)(?:[ ]|$)", cpuinfo, re.MULTILINE):
        raise RuntimeError("the host does not expose nested virtualization")


def find_module(root: Path, stem: str) -> Path | None:
    for suffix in (".ko", ".ko.xz", ".ko.zst"):
        match = next(root.rglob(stem + suffix), None)
        if match is not None:
            return match
    return None


def unpack_module(source: Path, destination: Path) -> None:
    if source.suffix == ".xz":
        with lzma.open(source, "rb") as compressed, destination.open("wb") as output:
            shutil.copyfileobj(compressed, output)
    elif source.suffix == ".zst":
        with destination.open("wb") as output:
            run(["zstd", "-d", "-q", "-c", str(source)], stdout=output)
    else:
        shutil.copy2(source, destination)


def module_dependencies(modprobe: Path, release: str) -> list[Path]:
    modules: list[Path] = []
    for name in ("virtio_pci", "virtiofs"):
        output = captured([str(modprobe), "-S", release, "--show-depends", name])
        for line in output.splitlines():
            fields = shlex.split(line)
            if len(fields) >= 2 and fields[0] == "insmod":
                modules.append(Path(fields[1]))
    return modules


def run_native() -> None:
    release = os.environ.get("MINIDOX_TEST_KERNEL_RELEASE", os.uname().release)
    kernel = Path(os.environ.get("MINIDOX_TEST_KERNEL", f"/boot/vmlinuz-{release}"))
    module_root = Path("/lib/modules") / release
    modprobe = Path(os.environ.get("MINIDOX_TEST_MODPROBE", "/sbin/modprobe"))
    virtiofs = Path(os.environ["MINIDOX_TEST_VIRTIOFS_MODULE"]) if os.environ.get(
        "MINIDOX_TEST_VIRTIOFS_MODULE"
    ) else find_module(module_root, "virtiofs")
    fuse = Path(os.environ["MINIDOX_TEST_FUSE_MODULE"]) if os.environ.get(
        "MINIDOX_TEST_FUSE_MODULE"
    ) else find_module(module_root, "fuse")

    kvm = Path("/dev/kvm")
    if not kvm.exists() or not stat.S_ISCHR(kvm.stat().st_mode):
        raise RuntimeError("/dev/kvm is not a character device")
    if not kernel.is_file():
        raise RuntimeError(f"guest kernel is not readable: {kernel}")
    if virtiofs is None or not virtiofs.is_file():
        raise RuntimeError("could not find the virtiofs kernel module")
    if not modprobe.is_file() or not os.access(modprobe, os.X_OK):
        raise RuntimeError(f"modprobe is not executable: {modprobe}")

    with tempfile.TemporaryDirectory(prefix="minidox-gce-test-") as temporary:
        root = Path(temporary)
        fixture = root / "initramfs"
        fixture.mkdir()
        initramfs = root / "initramfs.cpio"
        console = root / "console.log"
        console.touch()
        target = root / "target"
        succeeded = False
        try:
            run(
                [
                    "gcc",
                    "-static",
                    "-Os",
                    "-s",
                    "-o",
                    str(fixture / "init"),
                    str(REPO_ROOT / "tests/guest/virtiofs-dax-init.c"),
                ]
            )
            sources = module_dependencies(modprobe, release)
            if not sources:
                sources = ([fuse] if fuse is not None else []) + [virtiofs]
            manifest: list[str] = []
            for index, source in enumerate(sources):
                name = f"module-{index}.ko"
                unpack_module(source, fixture / name)
                manifest.append(f"/{name}\n")
            (fixture / "modules").write_text("".join(manifest))
            members = "".join(f"{path.name}\n" for path in fixture.iterdir()).encode()
            run(
                ["cpio", "--quiet", "-o", "-H", "newc", "-F", str(initramfs)],
                cwd=fixture,
                stdin=members,
            )

            environment = dict(
                os.environ,
                PATH=f"{Path.home() / '.cargo/bin'}:{os.environ.get('PATH', '')}",
                CARGO_TARGET_DIR=str(target),
                MINIDOX_TEST_KERNEL=str(kernel),
                MINIDOX_TEST_INITRAMFS=str(initramfs),
                MINIDOX_TEST_CONSOLE=str(console),
                MINIDOX_TEST_VIRTIOFS="1",
            )
            run(
                [
                    "cargo",
                    "test",
                    "--manifest-path",
                    str(REPO_ROOT / "Cargo.toml"),
                    "-p",
                    "minidox-vmm",
                    "--tests",
                    "--locked",
                    "--",
                    "--nocapture",
                ],
                env=environment,
            )
            succeeded = True
        finally:
            if not succeeded:
                print("guest console:", file=sys.stderr)
                if console.exists():
                    lines = console.read_text(errors="replace").splitlines()
                    print("\n".join(lines[-200:]), file=sys.stderr)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--native",
        action="store_true",
        help="run directly on the current Linux KVM host",
    )
    mode.add_argument(
        "--setup-host",
        metavar="USER",
        help=argparse.SUPPRESS,
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    signal.signal(signal.SIGTERM, lambda _signum, _frame: sys.exit(128 + signal.SIGTERM))
    if args.setup_host:
        setup_host(args.setup_host)
    elif args.native:
        run_native()
    else:
        run_on_gce()


if __name__ == "__main__":
    main()
