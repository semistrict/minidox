#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use minidox_vmm::{KvmSupervisor, RAM_PAGE_SIZE, VmConfig};
use serde_json::json;

const RAM_SIZE: usize = 512 << 20;

#[test]
fn supervisor_forks_running_vm_ram_redoxfs_and_machine_state() {
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
            "cmdline": "console=ttyAMA0 earlycon=pl011,0x09000000 panic=-1"
        },
        "memory": { "size": RAM_SIZE },
        "console": { "mode": "Null" },
        "serial": { "mode": "File", "file": console.to_string_lossy() }
    }))
    .unwrap();

    let mut supervisor = KvmSupervisor::new().unwrap();
    let source = supervisor.create_cloud_vm(config).unwrap();
    wait_for_console(&console, "Linux version", Duration::from_secs(180));

    let ram_gpa = wait_for_guest_ram_gpa(&console, Duration::from_secs(180));
    let memory_offset = usize::try_from(ram_gpa - guest_ram_start()).unwrap();
    assert!(memory_offset + RAM_PAGE_SIZE <= RAM_SIZE);
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
    let _cold = supervisor
        .create_file(source, "cold", RAM_PAGE_SIZE as u64)
        .unwrap();
    assert_eq!(
        supervisor.read_memory(source, memory_offset, 11).unwrap(),
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
        supervisor
            .write_memory(source, memory_offset + 64, b"cold fault")
            .unwrap();
        supervisor
            .write_memory(child, memory_offset + 64, b"cold fault")
            .unwrap();
        wait_for_shared_filesystem_pages(&supervisor, 2, 2, Duration::from_secs(180));
    }

    supervisor
        .write_memory(source, memory_offset, b"after fork!")
        .unwrap();
    supervisor
        .write_file(source, state, 128, b"after fork!")
        .unwrap();
    assert_eq!(
        supervisor.read_memory(child, memory_offset, 11).unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(child, state, 128, 11).unwrap(),
        b"before fork"
    );

    let grandchild = supervisor.fork_vm(child).unwrap();
    supervisor.remove_vm(source).unwrap();
    supervisor.remove_vm(child).unwrap();
    assert_eq!(
        supervisor
            .read_memory(grandchild, memory_offset, 11)
            .unwrap(),
        b"before fork"
    );
    assert_eq!(
        supervisor.read_file(grandchild, state, 128, 11).unwrap(),
        b"before fork"
    );

    supervisor.remove_vm(grandchild).unwrap();
    fs::remove_file(console).unwrap();
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

fn wait_for_guest_ram_gpa(path: &PathBuf, timeout: Duration) -> u64 {
    let started = std::time::Instant::now();
    while started.elapsed() < timeout {
        let contents = fs::read_to_string(path).unwrap_or_default();
        if let Some(value) = contents.lines().find_map(|line| {
            line.strip_prefix("MINIDOX_RAM_GPA=")
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
    panic!("guest console did not report a RAM GPA within {timeout:?}");
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
