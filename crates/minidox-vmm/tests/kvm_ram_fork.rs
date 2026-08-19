#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;

use cloud_hypervisor_hypervisor::{HypervisorVmConfig, Vcpu, Vm, VmExit};
use minidox_vmm::{KvmGuestRam, RAM_PAGE_SIZE};

const SLOT: u32 = 0;
const GPA: u64 = 0;
const DATA_OFFSET: usize = RAM_PAGE_SIZE;

#[test]
fn kvm_dirty_pages_form_recursive_cow_ram_branches() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM RAM fork test: /dev/kvm is unavailable");
        return;
    }

    let hypervisor = cloud_hypervisor_hypervisor::new().unwrap();
    let source_vm = hypervisor.create_vm(HypervisorVmConfig::default()).unwrap();
    let mut source_ram = KvmGuestRam::new(RAM_PAGE_SIZE * 3).unwrap();
    source_ram.write(0, &guest_program(42)).unwrap();
    source_ram.register(source_vm.as_ref(), SLOT, GPA).unwrap();
    let mut vcpu = create_vcpu(&source_vm);
    run_from_start(vcpu.as_mut());
    assert_eq!(read_value(&source_ram), 42);

    let child = source_ram.fork(source_vm.as_ref(), SLOT, GPA).unwrap();
    assert_eq!(read_value(&child), 42);
    let first_fork = KvmGuestRam::page_accounting([&source_ram, &child]);
    assert_eq!(first_fork.resident_pages, 3);
    assert_eq!(first_fork.shared_pages, 3);
    assert_eq!(first_fork.backing_files, 1);

    source_ram.write(0, &guest_program(84)).unwrap();
    run_from_start(vcpu.as_mut());
    assert_eq!(read_value(&source_ram), 84);
    assert_eq!(read_value(&child), 42);

    let grandchild = source_ram.fork(source_vm.as_ref(), SLOT, GPA).unwrap();
    assert_eq!(read_value(&grandchild), 84);
    assert_eq!(read_value(&child), 42);
    let recursive = KvmGuestRam::page_accounting([&source_ram, &child, &grandchild]);
    assert_eq!(recursive.resident_pages, 5);
    assert_eq!(recursive.shared_pages, 3);
    assert_eq!(recursive.backing_files, 2);

    source_ram
        .unregister(source_vm.as_ref(), SLOT, GPA)
        .unwrap();
}

fn read_value(ram: &KvmGuestRam) -> u64 {
    let mut bytes = [0; 8];
    ram.read(DATA_OFFSET, &mut bytes).unwrap();
    u64::from_le_bytes(bytes)
}

fn create_vcpu(vm: &Arc<dyn Vm>) -> Box<dyn Vcpu> {
    let vcpu = vm
        .create_vcpu(
            0,
            None,
            #[cfg(target_arch = "x86_64")]
            None,
        )
        .unwrap();

    #[cfg(target_arch = "aarch64")]
    {
        let mut init = vcpu.create_vcpu_init();
        vm.get_preferred_target(&mut init).unwrap();
        vcpu.vcpu_init(&init).unwrap();
    }

    vcpu
}

fn run_from_start(vcpu: &mut dyn Vcpu) {
    let mut regs = vcpu.get_regs().unwrap();

    #[cfg(target_arch = "x86_64")]
    {
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_sregs(&sregs).unwrap();
        regs.set_rip(0);
        regs.set_rflags(2);
    }

    #[cfg(target_arch = "aarch64")]
    {
        regs.set_pc(0);
        regs.set_pstate(0x3c5);
    }

    vcpu.set_regs(&regs).unwrap();
    assert!(matches!(
        vcpu.run().unwrap(),
        VmExit::Ignore | VmExit::Reset | VmExit::Shutdown
    ));
}

#[cfg(target_arch = "x86_64")]
fn guest_program(value: u32) -> Vec<u8> {
    let mut code = vec![
        0x48, 0xc7, 0xc0, 0x00, 0x10, 0x00, 0x00, // mov rax, 0x1000
        0x48, 0xc7, 0xc3, // mov rbx, value
    ];
    code.extend_from_slice(&value.to_le_bytes());
    code.extend_from_slice(&[
        0x48, 0x89, 0x18, // mov [rax], rbx
        0xf4, // hlt
    ]);
    code
}

#[cfg(target_arch = "aarch64")]
fn guest_program(value: u32) -> Vec<u8> {
    [
        0xd282_0000_u32,                // mov x0, #0x1000
        0xd280_0001_u32 | (value << 5), // mov x1, #value
        0xf900_0001_u32,                // str x1, [x0]
        0xd286_0002_u32,                // mov x2, #0x3000 (unmapped)
        0xf900_0041_u32,                // str x1, [x2]
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect()
}
