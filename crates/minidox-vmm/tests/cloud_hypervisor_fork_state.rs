#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use minidox_vmm::{KvmSupervisor, RAM_PAGE_SIZE, VmConfig};
use serde_json::json;

const RAM_SIZE: usize = 512 << 20;
const COMMAND_OFFSET: usize = 64;
const EXPECTED_OFFSET: usize = 80;
const RESPONSE_OFFSET: usize = 96;
const CONTROL_REGION_LEN: usize = 48;

#[test]
fn supervisor_forks_live_guest_processes_with_isolated_cow_filesystems() {
    let Ok(kernel) = env::var("MINIDOX_TEST_KERNEL") else {
        eprintln!("skipping nested lifecycle test: MINIDOX_TEST_KERNEL is unset");
        return;
    };
    let Ok(initramfs) = env::var("MINIDOX_TEST_INITRAMFS") else {
        eprintln!("skipping nested lifecycle test: MINIDOX_TEST_INITRAMFS is unset");
        return;
    };
    let console = env::var_os("MINIDOX_TEST_CONSOLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "/tmp/minidox-fork-state-console-{}.log",
                std::process::id()
            ))
        });
    let config: VmConfig = serde_json::from_value(json!({
        "payload": {
            "kernel": kernel,
            "initramfs": initramfs,
            "cmdline": guest_kernel_cmdline()
        },
        "memory": { "size": RAM_SIZE },
        "console": { "mode": "Null" },
        "serial": { "mode": "File", "file": console.to_string_lossy() }
    }))
    .unwrap();

    let storage = tempfile::tempdir().unwrap();
    let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
    let source = supervisor.create_cloud_vm(config).unwrap();
    wait_for_console(&console, "Linux version", Duration::from_secs(180));

    let parent_gpa = wait_for_guest_gpa(&console, "MINIDOX_RAM_GPA=", Duration::from_secs(180));
    let parent_offset = guest_memory_offset(parent_gpa);
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        wait_for_console(
            &console,
            "MINIDOX_VIRTIOFS_MOUNT_OK",
            Duration::from_secs(180),
        );
    }
    let state = supervisor
        .create_file(source, "state", RAM_PAGE_SIZE as u64)
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"before fork")
        .unwrap();
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        wait_for_console(
            &console,
            "MINIDOX_VIRTIOFS_DAX_OK",
            Duration::from_secs(180),
        );
    }
    let worker_gpa = wait_for_guest_gpa(&console, "MINIDOX_WORKER_GPA=", Duration::from_secs(180));
    let worker_offset = guest_memory_offset(worker_gpa);
    assert_ne!(parent_offset, worker_offset);
    let _cold = supervisor
        .create_file(source, "cold", RAM_PAGE_SIZE as u64)
        .unwrap();
    assert_eq!(
        supervisor.read_memory(source, parent_offset, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_memory(source, worker_offset, 11).unwrap(),
        b"before fork"
    );

    let child = supervisor.fork_vm(source).unwrap();
    let pause = supervisor.last_fork_pause().unwrap();
    assert!(
        pause < Duration::from_millis(300),
        "source pause was {pause:?}"
    );
    let shared = supervisor.page_accounting();
    assert_eq!(shared.memory.resident_pages, RAM_SIZE / RAM_PAGE_SIZE);
    assert_eq!(shared.memory.shared_pages, RAM_SIZE / RAM_PAGE_SIZE);
    assert_eq!(shared.filesystem.resident_pages, 1);
    assert_eq!(shared.filesystem.shared_pages, 1);

    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        for (vm, process_offset) in [
            (source, parent_offset),
            (source, worker_offset),
            (child, parent_offset),
            (child, worker_offset),
        ] {
            issue_guest_command(
                &mut supervisor,
                vm,
                process_offset,
                b"cold fault",
                b"",
                b"cold ok",
                Duration::from_secs(180),
            );
        }
        wait_for_shared_filesystem_pages(&supervisor, 2, 2, Duration::from_secs(180));
    }

    supervisor
        .write_memory(source, parent_offset, b"after fork!")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"after fork!")
        .unwrap();
    assert_eq!(
        supervisor.read_memory(child, parent_offset, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(child, state, 128, 11).unwrap(),
        b"before fork"
    );
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        assert_guest_processes_see(
            &mut supervisor,
            source,
            [parent_offset, worker_offset],
            b"after fork!",
        );
        assert_guest_processes_see(
            &mut supervisor,
            child,
            [parent_offset, worker_offset],
            b"before fork",
        );
    }

    supervisor
        .write_file(child, state, 128, b"child fork!")
        .unwrap();
    assert_eq!(
        supervisor.read_file(source, state, 128, 11).unwrap(),
        b"after fork!"
    );
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        assert_guest_processes_see(
            &mut supervisor,
            source,
            [parent_offset, worker_offset],
            b"after fork!",
        );
        assert_guest_processes_see(
            &mut supervisor,
            child,
            [parent_offset, worker_offset],
            b"child fork!",
        );
    }

    supervisor.checkpoint().unwrap();
    drop(supervisor);
    let mut supervisor = KvmSupervisor::open(storage.path()).unwrap();
    assert_eq!(
        supervisor.read_memory(source, parent_offset, 11).unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_memory(child, parent_offset, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(source, state, 128, 11).unwrap(),
        b"after fork!"
    );
    assert_eq!(
        supervisor.read_file(child, state, 128, 11).unwrap(),
        b"child fork!"
    );
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        assert_guest_processes_see(
            &mut supervisor,
            source,
            [parent_offset, worker_offset],
            b"after fork!",
        );
        assert_guest_processes_see(
            &mut supervisor,
            child,
            [parent_offset, worker_offset],
            b"child fork!",
        );
    }

    let grandchild = supervisor.fork_vm(child).unwrap();
    supervisor.remove_vm(source).unwrap();
    supervisor.remove_vm(child).unwrap();
    assert_eq!(
        supervisor
            .read_memory(grandchild, parent_offset, 11)
            .unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(grandchild, state, 128, 11).unwrap(),
        b"child fork!"
    );
    if env::var_os("MINIDOX_TEST_VIRTIOFS").is_some() {
        assert_guest_processes_see(
            &mut supervisor,
            grandchild,
            [parent_offset, worker_offset],
            b"child fork!",
        );
    }

    supervisor.remove_vm(grandchild).unwrap();
    fs::remove_file(console).unwrap();
}

fn assert_guest_processes_see(
    supervisor: &mut KvmSupervisor,
    vm: minidox_vmm::KvmVmId,
    process_offsets: [usize; 2],
    expected: &[u8],
) {
    for process_offset in process_offsets {
        issue_guest_command(
            supervisor,
            vm,
            process_offset,
            b"check state",
            expected,
            b"state ok",
            Duration::from_secs(30),
        );
    }
}

fn issue_guest_command(
    supervisor: &mut KvmSupervisor,
    vm: minidox_vmm::KvmVmId,
    process_offset: usize,
    command: &[u8],
    expected: &[u8],
    success: &[u8],
    timeout: Duration,
) {
    assert!(command.len() <= EXPECTED_OFFSET - COMMAND_OFFSET);
    assert!(expected.len() <= RESPONSE_OFFSET - EXPECTED_OFFSET);
    let mut region = [0_u8; CONTROL_REGION_LEN];
    region[..command.len()].copy_from_slice(command);
    let expected_start = EXPECTED_OFFSET - COMMAND_OFFSET;
    region[expected_start..expected_start + expected.len()].copy_from_slice(expected);
    supervisor
        .write_memory(vm, process_offset + COMMAND_OFFSET, &region)
        .unwrap();

    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let response = supervisor
            .read_memory(vm, process_offset + RESPONSE_OFFSET, 16)
            .unwrap();
        if response.starts_with(success) {
            return;
        }
        if response.starts_with(b"state bad") || response.starts_with(b"cold bad") {
            panic!(
                "guest process at RAM offset {process_offset:#x} rejected command {:?}: {:?}",
                String::from_utf8_lossy(command),
                String::from_utf8_lossy(&response)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "guest process at RAM offset {process_offset:#x} did not complete command {:?} within {timeout:?}",
        String::from_utf8_lossy(command)
    );
}

fn wait_for_shared_filesystem_pages(
    supervisor: &KvmSupervisor,
    resident_pages: usize,
    shared_pages: usize,
    timeout: Duration,
) {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let filesystem = supervisor.page_accounting().filesystem;
        if filesystem.resident_pages == resident_pages && filesystem.shared_pages == shared_pages {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let filesystem = supervisor.page_accounting().filesystem;
    panic!(
        "filesystem cache did not reach {resident_pages} resident and {shared_pages} shared pages within {timeout:?}; observed {filesystem:?}"
    );
}

fn wait_for_console(path: &PathBuf, marker: &str, timeout: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let contents = fs::read_to_string(path).unwrap_or_default();
        if contents.contains(marker) {
            return;
        }
        if let Some(line) = contents
            .lines()
            .find(|line| line.contains("MINIDOX_VIRTIOFS_") && line.contains("ERROR"))
        {
            panic!("guest virtio-fs verifier failed: {line}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let contents = fs::read_to_string(path).unwrap_or_default();
    let tail = contents.lines().rev().take(80).collect::<Vec<_>>();
    panic!(
        "guest console did not contain {marker:?} within {timeout:?}; console tail:\n{}",
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    );
}

fn wait_for_guest_gpa(path: &PathBuf, marker: &str, timeout: Duration) -> u64 {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let contents = fs::read_to_string(path).unwrap_or_default();
        if let Some(value) = contents.lines().find_map(|line| {
            line.strip_prefix(marker)
                .and_then(|value| value.strip_prefix("0x"))
                .and_then(|value| u64::from_str_radix(value, 16).ok())
        }) {
            return value;
        }
        if let Some(line) = contents
            .lines()
            .find(|line| line.contains("MINIDOX_RAM_") && line.contains("ERROR"))
        {
            panic!("guest RAM verifier failed: {line}");
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("guest console did not report {marker:?} within {timeout:?}");
}

fn guest_memory_offset(gpa: u64) -> usize {
    let offset = usize::try_from(gpa - guest_ram_start()).unwrap();
    assert!(offset + RAM_PAGE_SIZE <= RAM_SIZE);
    offset
}

const fn guest_ram_start() -> u64 {
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    {
        0x4000_0000
    }
    #[cfg(target_arch = "x86_64")]
    {
        0
    }
}

const fn guest_kernel_cmdline() -> &'static str {
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    {
        "console=ttyAMA0 earlycon=pl011,0x09000000 panic=-1"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "console=ttyS0 earlyprintk=ttyS0 panic=-1"
    }
}
