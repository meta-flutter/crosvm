// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Virtual machine architecture support code.

pub mod android;
pub mod fdt;
pub mod pstore;
pub mod serial;

pub mod sys;

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{
    mpsc::{self, SendError},
    Arc,
};

use acpi_tables::sdt::SDT;
use base::syslog;
use base::AsRawDescriptor;
use base::AsRawDescriptors;
use base::Event;
use base::SendTube;
#[cfg(all(target_arch = "x86_64", feature = "gdb"))]
use base::Tube;
use devices::virtio::VirtioDevice;
use devices::BarRange;
use devices::Bus;
use devices::BusDevice;
use devices::BusDeviceObj;
use devices::BusError;
use devices::BusResumeDevice;
use devices::HotPlugBus;
use devices::IrqChip;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use devices::IrqChipAArch64 as IrqChipArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use devices::IrqChipX86_64 as IrqChipArch;
use devices::IrqEventSource;
#[cfg(windows)]
use devices::Minijail;
use devices::PciAddress;
use devices::PciBus;
use devices::PciDevice;
use devices::PciDeviceError;
use devices::PciInterruptPin;
use devices::PciRoot;
use devices::PciRootCommand;
#[cfg(unix)]
use devices::ProxyDevice;
use devices::SerialHardware;
use devices::SerialParameters;
#[cfg(all(target_arch = "x86_64", feature = "gdb"))]
use gdbstub_arch::x86::reg::X86_64CoreRegs as GdbStubRegs;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::CpuConfigAArch64 as CpuConfigArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::CpuConfigX86_64 as CpuConfigArch;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::Hypervisor as HypervisorArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::HypervisorX86_64 as HypervisorArch;
use hypervisor::IoEventAddress;
use hypervisor::ProtectionType;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::VcpuAArch64 as VcpuArch;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::VcpuInitAArch64 as VcpuInitArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::VcpuInitX86_64 as VcpuInitArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::VcpuX86_64 as VcpuArch;
use hypervisor::Vm;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::VmAArch64 as VmArch;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::VmX86_64 as VmArch;
#[cfg(unix)]
use minijail::Minijail;
use remain::sorted;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use resources::AddressRange;
use resources::SystemAllocator;
use resources::SystemAllocatorConfig;
use serde::Deserialize;
use serde::Serialize;
pub use serial::add_serial_devices;
pub use serial::get_serial_cmdline;
pub use serial::set_default_serial_parameters;
pub use serial::GetSerialCmdlineError;
pub use serial::SERIAL_ADDR;
use sync::Mutex;
use thiserror::Error;
use vm_control::BatControl;
use vm_control::BatteryType;
use vm_control::PmResource;
use vm_memory::GuestAddress;
use vm_memory::GuestMemory;
use vm_memory::GuestMemoryError;

pub enum VmImage {
    Kernel(File),
    Bios(File),
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Pstore {
    pub path: PathBuf,
    pub size: u32,
}

/// Mapping of guest VCPU threads to host CPU cores.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum VcpuAffinity {
    /// All VCPU threads will be pinned to the same set of host CPU cores.
    Global(Vec<usize>),
    /// Each VCPU may be pinned to a set of host CPU cores.
    /// The map key is a guest VCPU index, and the corresponding value is the set of
    /// host CPU indices that the VCPU thread will be allowed to run on.
    /// If a VCPU index is not present in the map, its affinity will not be set.
    PerVcpu(BTreeMap<usize, Vec<usize>>),
}

/// Holds the pieces needed to build a VM. Passed to `build_vm` in the `LinuxArch` trait below to
/// create a `RunnableLinuxVm`.
#[sorted]
pub struct VmComponents {
    pub acpi_sdts: Vec<SDT>,
    pub android_fstab: Option<File>,
    pub cpu_capacity: BTreeMap<usize, u32>,
    pub cpu_clusters: Vec<Vec<usize>>,
    pub delay_rt: bool,
    #[cfg(feature = "direct")]
    pub direct_gpe: Vec<u32>,
    pub dmi_path: Option<PathBuf>,
    pub extra_kernel_params: Vec<String>,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub force_s2idle: bool,
    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    pub gdb: Option<(u32, Tube)>, // port and control tube.
    pub host_cpu_topology: bool,
    pub hugepages: bool,
    pub initrd_image: Option<File>,
    pub itmt: bool,
    pub memory_size: u64,
    pub no_i8042: bool,
    pub no_rtc: bool,
    pub no_smt: bool,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub pci_low_start: Option<u64>,
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    pub pcie_ecam: Option<AddressRange>,
    pub pflash_block_size: u32,
    pub pflash_image: Option<File>,
    pub protected_vm: ProtectionType,
    pub pstore: Option<Pstore>,
    /// A file to load as pVM firmware. Must be `Some` iff
    /// `protected_vm == ProtectionType::UnprotectedWithFirmware`.
    pub pvm_fw: Option<File>,
    pub rt_cpus: Vec<usize>,
    pub swiotlb: Option<u64>,
    pub vcpu_affinity: Option<VcpuAffinity>,
    pub vcpu_count: usize,
    pub vm_image: VmImage,
}

/// Holds the elements needed to run a Linux VM. Created by `build_vm`.
#[sorted]
pub struct RunnableLinuxVm<V: VmArch, Vcpu: VcpuArch> {
    pub bat_control: Option<BatControl>,
    pub delay_rt: bool,
    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    pub gdb: Option<(u32, Tube)>,
    pub has_bios: bool,
    pub hotplug_bus: Vec<Arc<Mutex<dyn HotPlugBus>>>,
    pub io_bus: Arc<Bus>,
    pub irq_chip: Box<dyn IrqChipArch>,
    pub mmio_bus: Arc<Bus>,
    pub no_smt: bool,
    pub pid_debug_label_map: BTreeMap<u32, String>,
    pub pm: Option<Arc<Mutex<dyn PmResource>>>,
    /// Devices to be notified before the system resumes from the S3 suspended state.
    pub resume_notify_devices: Vec<Arc<Mutex<dyn BusResumeDevice>>>,
    pub root_config: Arc<Mutex<PciRoot>>,
    pub rt_cpus: Vec<usize>,
    pub suspend_evt: Event,
    pub vcpu_affinity: Option<VcpuAffinity>,
    pub vcpu_count: usize,
    pub vcpu_init: Vec<VcpuInitArch>,
    /// If vcpus is None, then it's the responsibility of the vcpu thread to create vcpus.
    /// If it's Some, then `build_vm` already created the vcpus.
    pub vcpus: Option<Vec<Vcpu>>,
    pub vm: V,
}

/// The device and optional jail.
pub struct VirtioDeviceStub {
    pub dev: Box<dyn VirtioDevice>,
    pub jail: Option<Minijail>,
}

/// Trait which is implemented for each Linux Architecture in order to
/// set up the memory, cpus, and system devices and to boot the kernel.
pub trait LinuxArch {
    type Error: StdError;

    /// Returns a Vec of the valid memory addresses as pairs of address and length. These should be
    /// used to configure the `GuestMemory` structure for the platform.
    ///
    /// # Arguments
    ///
    /// * `components` - Parts used to determine the memory layout.
    fn guest_memory_layout(
        components: &VmComponents,
    ) -> std::result::Result<Vec<(GuestAddress, u64)>, Self::Error>;

    /// Gets the configuration for a new `SystemAllocator` that fits the given `Vm`'s memory layout.
    ///
    /// This is the per-architecture template for constructing the `SystemAllocator`. Platform
    /// agnostic modifications may be made to this configuration, but the final `SystemAllocator`
    /// will be at least as strict as this configuration.
    ///
    /// # Arguments
    ///
    /// * `vm` - The virtual machine to be used as a template for the `SystemAllocator`.
    fn get_system_allocator_config<V: Vm>(vm: &V) -> SystemAllocatorConfig;

    /// Takes `VmComponents` and generates a `RunnableLinuxVm`.
    ///
    /// # Arguments
    ///
    /// * `components` - Parts to use to build the VM.
    /// * `vm_evt_wrtube` - Tube used by sub-devices to request that crosvm exit because guest
    ///     wants to stop/shut down or requested reset.
    /// * `system_allocator` - Allocator created by this trait's implementation of
    ///   `get_system_allocator_config`.
    /// * `serial_parameters` - Definitions for how the serial devices should be configured.
    /// * `serial_jail` - Jail used for serial devices created here.
    /// * `battery` - Defines what battery device will be created.
    /// * `vm` - A VM implementation to build upon.
    /// * `ramoops_region` - Region allocated for ramoops.
    /// * `devices` - The devices to be built into the VM.
    /// * `irq_chip` - The IRQ chip implemention for the VM.
    /// * `debugcon_jail` - Jail used for debugcon devices created here.
    /// * `pflash_jail` - Jail used for pflash device created here.
    fn build_vm<V, Vcpu>(
        components: VmComponents,
        vm_evt_wrtube: &SendTube,
        system_allocator: &mut SystemAllocator,
        serial_parameters: &BTreeMap<(SerialHardware, u8), SerialParameters>,
        serial_jail: Option<Minijail>,
        battery: (Option<BatteryType>, Option<Minijail>),
        vm: V,
        ramoops_region: Option<pstore::RamoopsRegion>,
        devices: Vec<(Box<dyn BusDeviceObj>, Option<Minijail>)>,
        irq_chip: &mut dyn IrqChipArch,
        vcpu_ids: &mut Vec<usize>,
        debugcon_jail: Option<Minijail>,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] pflash_jail: Option<Minijail>,
    ) -> std::result::Result<RunnableLinuxVm<V, Vcpu>, Self::Error>
    where
        V: VmArch,
        Vcpu: VcpuArch;

    /// Configures the vcpu and should be called once per vcpu from the vcpu's thread.
    ///
    /// # Arguments
    ///
    /// * `vm` - The virtual machine object.
    /// * `hypervisor` - The `Hypervisor` that created the vcpu.
    /// * `irq_chip` - The `IrqChip` associated with this vm.
    /// * `vcpu` - The VCPU object to configure.
    /// * `vcpu_init` - The data required to initialize VCPU registers and other state.
    /// * `vcpu_id` - The id of the given `vcpu`.
    /// * `num_cpus` - Number of virtual CPUs the guest will have.
    /// * `has_bios` - Whether the `VmImage` is a `Bios` image
    /// * `cpu_config` - CPU feature configurations.
    fn configure_vcpu<V: Vm>(
        vm: &V,
        hypervisor: &dyn HypervisorArch,
        irq_chip: &mut dyn IrqChipArch,
        vcpu: &mut dyn VcpuArch,
        vcpu_init: VcpuInitArch,
        vcpu_id: usize,
        num_cpus: usize,
        has_bios: bool,
        cpu_config: Option<CpuConfigArch>,
    ) -> Result<(), Self::Error>;

    /// Configures and add a pci device into vm
    fn register_pci_device<V: VmArch, Vcpu: VcpuArch>(
        linux: &mut RunnableLinuxVm<V, Vcpu>,
        device: Box<dyn PciDevice>,
        #[cfg(unix)] minijail: Option<Minijail>,
        resources: &mut SystemAllocator,
        hp_control_tube: &mpsc::Sender<PciRootCommand>,
    ) -> Result<PciAddress, Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Reads vCPU's registers.
    fn debug_read_registers<T: VcpuArch>(vcpu: &T) -> Result<GdbStubRegs, Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Writes vCPU's registers.
    fn debug_write_registers<T: VcpuArch>(vcpu: &T, regs: &GdbStubRegs) -> Result<(), Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Reads bytes from the guest memory.
    fn debug_read_memory<T: VcpuArch>(
        vcpu: &T,
        guest_mem: &GuestMemory,
        vaddr: GuestAddress,
        len: usize,
    ) -> Result<Vec<u8>, Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Writes bytes to the specified guest memory.
    fn debug_write_memory<T: VcpuArch>(
        vcpu: &T,
        guest_mem: &GuestMemory,
        vaddr: GuestAddress,
        buf: &[u8],
    ) -> Result<(), Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Make the next vCPU's run single-step.
    fn debug_enable_singlestep<T: VcpuArch>(vcpu: &T) -> Result<(), Self::Error>;

    #[cfg(all(target_arch = "x86_64", feature = "gdb"))]
    /// Set hardware breakpoints at the given addresses.
    fn debug_set_hw_breakpoints<T: VcpuArch>(
        vcpu: &T,
        breakpoints: &[GuestAddress],
    ) -> Result<(), Self::Error>;
}

/// Errors for device manager.
#[sorted]
#[derive(Error, Debug)]
pub enum DeviceRegistrationError {
    /// No more MMIO space available.
    #[error("no more addresses are available")]
    AddrsExhausted,
    /// Could not allocate device address space for the device.
    #[error("Allocating device addresses: {0}")]
    AllocateDeviceAddrs(PciDeviceError),
    /// Could not allocate IO space for the device.
    #[error("Allocating IO addresses: {0}")]
    AllocateIoAddrs(PciDeviceError),
    /// Could not allocate MMIO or IO resource for the device.
    #[error("Allocating IO resource: {0}")]
    AllocateIoResource(resources::Error),
    /// Could not allocate an IRQ number.
    #[error("Allocating IRQ number")]
    AllocateIrq,
    /// Could not allocate IRQ resource for the device.
    #[cfg(unix)]
    #[error("Allocating IRQ resource: {0}")]
    AllocateIrqResource(devices::vfio::VfioError),
    /// Broken pci topology
    #[error("pci topology is broken")]
    BrokenPciTopology,
    /// Unable to clone a jail for the device.
    #[cfg(unix)]
    #[error("failed to clone jail: {0}")]
    CloneJail(minijail::Error),
    /// Appending to kernel command line failed.
    #[error("unable to add device to kernel command line: {0}")]
    Cmdline(kernel_cmdline::Error),
    /// Configure window size failed.
    #[error("failed to configure window size: {0}")]
    ConfigureWindowSize(PciDeviceError),
    // Unable to create a pipe.
    #[error("failed to create pipe: {0}")]
    CreatePipe(base::Error),
    // Unable to create serial device from serial parameters
    #[error("failed to create serial device: {0}")]
    CreateSerialDevice(devices::SerialError),
    // Unable to create tube
    #[error("failed to create tube: {0}")]
    CreateTube(base::TubeError),
    /// Could not clone an event.
    #[error("failed to clone event: {0}")]
    EventClone(base::Error),
    /// Could not create an event.
    #[error("failed to create event: {0}")]
    EventCreate(base::Error),
    /// No more IRQs are available.
    #[error("no more IRQs are available")]
    IrqsExhausted,
    /// Missing a required serial device.
    #[error("missing required serial device {0}")]
    MissingRequiredSerialDevice(u8),
    /// Could not add a device to the mmio bus.
    #[error("failed to add to mmio bus: {0}")]
    MmioInsert(BusError),
    #[cfg(unix)]
    /// Failed to initialize proxy device for jailed device.
    #[error("failed to create proxy device: {0}")]
    ProxyDeviceCreation(devices::ProxyError),
    #[cfg(unix)]
    /// Failed to register battery device.
    #[error("failed to register battery device to VM: {0}")]
    RegisterBattery(devices::BatteryError),
    /// Could not register PCI device to pci root bus
    #[error("failed to register PCI device to pci root bus")]
    RegisterDevice(SendError<PciRootCommand>),
    /// Could not register PCI device capabilities.
    #[error("could not register PCI device capabilities: {0}")]
    RegisterDeviceCapabilities(PciDeviceError),
    /// Failed to register ioevent with VM.
    #[error("failed to register ioevent to VM: {0}")]
    RegisterIoevent(base::Error),
    /// Failed to register irq event with VM.
    #[error("failed to register irq event to VM: {0}")]
    RegisterIrqfd(base::Error),
    /// Could not setup VFIO platform IRQ for the device.
    #[error("Setting up VFIO platform IRQ: {0}")]
    SetupVfioPlatformIrq(anyhow::Error),
}

/// Config a PCI device for used by this vm.
pub fn configure_pci_device<V: VmArch, Vcpu: VcpuArch>(
    linux: &mut RunnableLinuxVm<V, Vcpu>,
    mut device: Box<dyn PciDevice>,
    #[cfg(unix)] jail: Option<Minijail>,
    resources: &mut SystemAllocator,
    hp_control_tube: &mpsc::Sender<PciRootCommand>,
) -> Result<PciAddress, DeviceRegistrationError> {
    // Allocate PCI device address before allocating BARs.
    let pci_address = device
        .allocate_address(resources)
        .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;

    // Allocate ranges that may need to be in the low MMIO region (MmioType::Low).
    let mmio_ranges = device
        .allocate_io_bars(resources)
        .map_err(DeviceRegistrationError::AllocateIoAddrs)?;

    // Allocate device ranges that may be in low or high MMIO after low-only ranges.
    let device_ranges = device
        .allocate_device_bars(resources)
        .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;

    // Do not suggest INTx for hot-plug devices.
    let intx_event = devices::IrqLevelEvent::new().map_err(DeviceRegistrationError::EventCreate)?;

    if let Some((gsi, _pin)) = device.assign_irq(&intx_event, None) {
        resources.reserve_irq(gsi);

        linux
            .irq_chip
            .as_irq_chip_mut()
            .register_level_irq_event(gsi, &intx_event, IrqEventSource::from_device(&device))
            .map_err(DeviceRegistrationError::RegisterIrqfd)?;
    }

    let mut keep_rds = device.keep_rds();
    syslog::push_descriptors(&mut keep_rds);

    device
        .register_device_capabilities()
        .map_err(DeviceRegistrationError::RegisterDeviceCapabilities)?;
    for (event, addr, datamatch) in device.ioevents() {
        let io_addr = IoEventAddress::Mmio(addr);
        linux
            .vm
            .register_ioevent(event, io_addr, datamatch)
            .map_err(DeviceRegistrationError::RegisterIoevent)?;
        keep_rds.push(event.as_raw_descriptor());
    }

    #[cfg(unix)]
    let arced_dev: Arc<Mutex<dyn BusDevice>> = if let Some(jail) = jail {
        let proxy = ProxyDevice::new(device, jail, keep_rds)
            .map_err(DeviceRegistrationError::ProxyDeviceCreation)?;
        linux
            .pid_debug_label_map
            .insert(proxy.pid() as u32, proxy.debug_label());
        Arc::new(Mutex::new(proxy))
    } else {
        device.on_sandboxed();
        Arc::new(Mutex::new(device))
    };

    #[cfg(windows)]
    let arced_dev = {
        device.on_sandboxed();
        Arc::new(Mutex::new(device))
    };

    #[cfg(unix)]
    hp_control_tube
        .send(PciRootCommand::Add(pci_address, arced_dev.clone()))
        .map_err(DeviceRegistrationError::RegisterDevice)?;

    for range in &mmio_ranges {
        linux
            .mmio_bus
            .insert(arced_dev.clone(), range.addr, range.size)
            .map_err(DeviceRegistrationError::MmioInsert)?;
    }

    for range in &device_ranges {
        linux
            .mmio_bus
            .insert(arced_dev.clone(), range.addr, range.size)
            .map_err(DeviceRegistrationError::MmioInsert)?;
    }

    Ok(pci_address)
}

// Generate pci topology starting from parent bus
pub fn generate_pci_topology(
    parent_bus: Arc<Mutex<PciBus>>,
    resources: &mut SystemAllocator,
    io_ranges: &mut BTreeMap<usize, Vec<BarRange>>,
    device_ranges: &mut BTreeMap<usize, Vec<BarRange>>,
    device_addrs: &[PciAddress],
    devices: &mut Vec<(Box<dyn PciDevice>, Option<Minijail>)>,
) -> Result<(Vec<BarRange>, u8), DeviceRegistrationError> {
    let mut bar_ranges = Vec::new();
    let bus_num = parent_bus.lock().get_bus_num();
    let mut subordinate_bus = bus_num;
    for (dev_idx, addr) in device_addrs.iter().enumerate() {
        // Only target for devices that located on this bus
        if addr.bus == bus_num {
            // If this device is a pci bridge (a.k.a., it has a pci bus structure),
            // create its topology recursively
            if let Some(child_bus) = devices[dev_idx].0.get_new_pci_bus() {
                let (child_bar_ranges, child_sub_bus) = generate_pci_topology(
                    child_bus.clone(),
                    resources,
                    io_ranges,
                    device_ranges,
                    device_addrs,
                    devices,
                )?;
                let device = &mut devices[dev_idx].0;
                parent_bus
                    .lock()
                    .add_child_bus(child_bus.clone())
                    .map_err(|_| DeviceRegistrationError::BrokenPciTopology)?;
                let bridge_window = device
                    .configure_bridge_window(resources, &child_bar_ranges)
                    .map_err(DeviceRegistrationError::ConfigureWindowSize)?;
                bar_ranges.extend(bridge_window);

                let ranges = device
                    .allocate_io_bars(resources)
                    .map_err(DeviceRegistrationError::AllocateIoAddrs)?;
                io_ranges.insert(dev_idx, ranges.clone());
                bar_ranges.extend(ranges);

                let ranges = device
                    .allocate_device_bars(resources)
                    .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;
                device_ranges.insert(dev_idx, ranges.clone());
                bar_ranges.extend(ranges);

                device.set_subordinate_bus(child_sub_bus);

                subordinate_bus = std::cmp::max(subordinate_bus, child_sub_bus);
            }
        }
    }

    for (dev_idx, addr) in device_addrs.iter().enumerate() {
        if addr.bus == bus_num {
            let device = &mut devices[dev_idx].0;
            // Allocate MMIO for non-bridge devices
            if device.get_new_pci_bus().is_none() {
                let ranges = device
                    .allocate_io_bars(resources)
                    .map_err(DeviceRegistrationError::AllocateIoAddrs)?;
                io_ranges.insert(dev_idx, ranges.clone());
                bar_ranges.extend(ranges);

                let ranges = device
                    .allocate_device_bars(resources)
                    .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;
                device_ranges.insert(dev_idx, ranges.clone());
                bar_ranges.extend(ranges);
            }
        }
    }
    Ok((bar_ranges, subordinate_bus))
}

/// Ensure all PCI devices have an assigned PCI address.
pub fn assign_pci_addresses(
    devices: &mut [(Box<dyn BusDeviceObj>, Option<Minijail>)],
    resources: &mut SystemAllocator,
) -> Result<(), DeviceRegistrationError> {
    // First allocate devices with a preferred address.
    for pci_device in devices
        .iter_mut()
        .filter_map(|(device, _jail)| device.as_pci_device_mut())
        .filter(|pci_device| pci_device.preferred_address().is_some())
    {
        let _ = pci_device
            .allocate_address(resources)
            .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;
    }

    // Then allocate addresses for the remaining devices.
    for pci_device in devices
        .iter_mut()
        .filter_map(|(device, _jail)| device.as_pci_device_mut())
        .filter(|pci_device| pci_device.preferred_address().is_none())
    {
        let _ = pci_device
            .allocate_address(resources)
            .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;
    }

    Ok(())
}

/// Creates a root PCI device for use by this Vm.
pub fn generate_pci_root(
    mut devices: Vec<(Box<dyn PciDevice>, Option<Minijail>)>,
    irq_chip: &mut dyn IrqChip,
    mmio_bus: Arc<Bus>,
    io_bus: Arc<Bus>,
    resources: &mut SystemAllocator,
    vm: &mut impl Vm,
    max_irqs: usize,
) -> Result<
    (
        PciRoot,
        Vec<(PciAddress, u32, PciInterruptPin)>,
        BTreeMap<u32, String>,
    ),
    DeviceRegistrationError,
> {
    let mut device_addrs = Vec::new();

    for (device, _jail) in devices.iter_mut() {
        let address = device
            .allocate_address(resources)
            .map_err(DeviceRegistrationError::AllocateDeviceAddrs)?;
        device_addrs.push(address);
    }

    let mut device_ranges = BTreeMap::new();
    let mut io_ranges = BTreeMap::new();
    let root_bus = Arc::new(Mutex::new(PciBus::new(0, 0, false)));

    generate_pci_topology(
        root_bus.clone(),
        resources,
        &mut io_ranges,
        &mut device_ranges,
        &device_addrs,
        &mut devices,
    )?;

    let mut root = PciRoot::new(Arc::downgrade(&mmio_bus), Arc::downgrade(&io_bus), root_bus);
    #[cfg_attr(windows, allow(unused_mut))]
    let mut pid_labels = BTreeMap::new();

    // Allocate legacy INTx
    let mut pci_irqs = Vec::new();
    let mut irqs: Vec<Option<u32>> = vec![None; max_irqs];

    for (dev_idx, (device, _jail)) in devices.iter_mut().enumerate() {
        // For default interrupt routing use next preallocated interrupt from the pool.
        let irq_num = if let Some(irq) = irqs[dev_idx % max_irqs] {
            irq
        } else {
            let irq = resources
                .allocate_irq()
                .ok_or(DeviceRegistrationError::AllocateIrq)?;
            irqs[dev_idx % max_irqs] = Some(irq);
            irq
        };

        let intx_event =
            devices::IrqLevelEvent::new().map_err(DeviceRegistrationError::EventCreate)?;

        if let Some((gsi, pin)) = device.assign_irq(&intx_event, Some(irq_num)) {
            // reserve INTx if needed and non-default.
            if gsi != irq_num {
                resources.reserve_irq(gsi);
            };
            irq_chip
                .register_level_irq_event(gsi, &intx_event, IrqEventSource::from_device(device))
                .map_err(DeviceRegistrationError::RegisterIrqfd)?;

            pci_irqs.push((device_addrs[dev_idx], gsi, pin));
        }
    }

    // To prevent issues where device's on_sandbox may spawn thread before all
    // sandboxed devices are sandboxed we partition iterator to go over sandboxed
    // first. This is needed on linux platforms. On windows, this is a no-op since
    // jails are always None, even for sandboxed devices.
    let devices = {
        let (sandboxed, non_sandboxed): (Vec<_>, Vec<_>) = devices
            .into_iter()
            .enumerate()
            .partition(|(_, (_, jail))| jail.is_some());
        sandboxed.into_iter().chain(non_sandboxed.into_iter())
    };
    for (dev_idx, dev_value) in devices {
        #[cfg(unix)]
        let (mut device, jail) = dev_value;
        #[cfg(windows)]
        let (mut device, _) = dev_value;
        let address = device_addrs[dev_idx];

        let mut keep_rds = device.keep_rds();
        syslog::push_descriptors(&mut keep_rds);
        keep_rds.append(&mut vm.get_memory().as_raw_descriptors());

        let ranges = io_ranges.remove(&dev_idx).unwrap_or_default();
        let device_ranges = device_ranges.remove(&dev_idx).unwrap_or_default();
        device
            .register_device_capabilities()
            .map_err(DeviceRegistrationError::RegisterDeviceCapabilities)?;
        for (event, addr, datamatch) in device.ioevents() {
            let io_addr = IoEventAddress::Mmio(addr);
            vm.register_ioevent(event, io_addr, datamatch)
                .map_err(DeviceRegistrationError::RegisterIoevent)?;
            keep_rds.push(event.as_raw_descriptor());
        }

        #[cfg(unix)]
        let arced_dev: Arc<Mutex<dyn BusDevice>> = if let Some(jail) = jail {
            let proxy = ProxyDevice::new(device, jail, keep_rds)
                .map_err(DeviceRegistrationError::ProxyDeviceCreation)?;
            pid_labels.insert(proxy.pid() as u32, proxy.debug_label());
            Arc::new(Mutex::new(proxy))
        } else {
            device.on_sandboxed();
            Arc::new(Mutex::new(device))
        };
        #[cfg(windows)]
        let arced_dev = {
            device.on_sandboxed();
            Arc::new(Mutex::new(device))
        };
        root.add_device(address, arced_dev.clone());
        for range in &ranges {
            mmio_bus
                .insert(arced_dev.clone(), range.addr, range.size)
                .map_err(DeviceRegistrationError::MmioInsert)?;
        }

        for range in &device_ranges {
            mmio_bus
                .insert(arced_dev.clone(), range.addr, range.size)
                .map_err(DeviceRegistrationError::MmioInsert)?;
        }
    }

    Ok((root, pci_irqs, pid_labels))
}

/// Errors for image loading.
#[sorted]
#[derive(Error, Debug)]
pub enum LoadImageError {
    #[error("Alignment not a power of two: {0}")]
    BadAlignment(u64),
    #[error("Image size too large: {0}")]
    ImageSizeTooLarge(u64),
    #[error("Reading image into memory failed: {0}")]
    ReadToMemory(GuestMemoryError),
    #[error("Seek failed: {0}")]
    Seek(io::Error),
}

/// Load an image from a file into guest memory.
///
/// # Arguments
///
/// * `guest_mem` - The memory to be used by the guest.
/// * `guest_addr` - The starting address to load the image in the guest memory.
/// * `max_size` - The amount of space in bytes available in the guest memory for the image.
/// * `image` - The file containing the image to be loaded.
///
/// The size in bytes of the loaded image is returned.
pub fn load_image<F>(
    guest_mem: &GuestMemory,
    image: &mut F,
    guest_addr: GuestAddress,
    max_size: u64,
) -> Result<usize, LoadImageError>
where
    F: Read + Seek + AsRawDescriptor,
{
    let size = image.seek(SeekFrom::End(0)).map_err(LoadImageError::Seek)?;

    if size > usize::max_value() as u64 || size > max_size {
        return Err(LoadImageError::ImageSizeTooLarge(size));
    }

    // This is safe due to the bounds check above.
    let size = size as usize;

    image
        .seek(SeekFrom::Start(0))
        .map_err(LoadImageError::Seek)?;

    guest_mem
        .read_to_memory(guest_addr, image, size)
        .map_err(LoadImageError::ReadToMemory)?;

    Ok(size)
}

/// Load an image from a file into guest memory at the highest possible address.
///
/// # Arguments
///
/// * `guest_mem` - The memory to be used by the guest.
/// * `image` - The file containing the image to be loaded.
/// * `min_guest_addr` - The minimum address of the start of the image.
/// * `max_guest_addr` - The address to load the last byte of the image.
/// * `align` - The minimum alignment of the start address of the image in bytes
///   (must be a power of two).
///
/// The guest address and size in bytes of the loaded image are returned.
pub fn load_image_high<F>(
    guest_mem: &GuestMemory,
    image: &mut F,
    min_guest_addr: GuestAddress,
    max_guest_addr: GuestAddress,
    align: u64,
) -> Result<(GuestAddress, usize), LoadImageError>
where
    F: Read + Seek + AsRawDescriptor,
{
    if !align.is_power_of_two() {
        return Err(LoadImageError::BadAlignment(align));
    }

    let max_size = max_guest_addr.offset_from(min_guest_addr) & !(align - 1);
    let size = image.seek(SeekFrom::End(0)).map_err(LoadImageError::Seek)?;

    if size > usize::max_value() as u64 || size > max_size {
        return Err(LoadImageError::ImageSizeTooLarge(size));
    }

    image
        .seek(SeekFrom::Start(0))
        .map_err(LoadImageError::Seek)?;

    // Load image at the maximum aligned address allowed.
    // The subtraction cannot underflow because of the size checks above.
    let guest_addr = GuestAddress((max_guest_addr.offset() - size) & !(align - 1));

    // This is safe due to the bounds check above.
    let size = size as usize;

    guest_mem
        .read_to_memory(guest_addr, image, size)
        .map_err(LoadImageError::ReadToMemory)?;

    Ok((guest_addr, size))
}

/// Read and write permissions setting
///
/// Wrap read_allow and write_allow to store them in MsrHandlers level.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum MsrRWType {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

/// Handler types for userspace-msr
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum MsrAction {
    /// Read and write from host directly, and the control of MSR will
    /// take effect on host.
    MsrPassthrough,
    /// Store the dummy value for msr (copy from host or custom values),
    /// and the control(WRMSR) of MSR won't take effect on host.
    MsrEmulate,
}

/// Source CPU of MSR value
///
/// Indicate which CPU that user get/set MSRs from/to.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum MsrValueFrom {
    /// Read/write MSR value from/into CPU 0.
    /// The MSR source CPU always be CPU 0.
    RWFromCPU0,
    /// Read/write MSR value from/into the running CPU.
    /// If vCPU migrates to another pcpu, the MSR source CPU will also change.
    RWFromRunningCPU,
}

/// Whether to force KVM-filtered MSRs.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub enum MsrFilter {
    /// Leave it to hypervisor (KVM) default.
    Default,
    /// Don't let KVM do the default thing and use our userspace MSR
    /// implementation.
    Override,
}

/// Config option for userspace-msr handing
///
/// MsrConfig will be collected with its corresponding MSR's index.
/// eg, (msr_index, msr_config)
#[derive(Clone, Serialize, Deserialize)]
pub struct MsrConfig {
    /// If support RDMSR/WRMSR emulation in crosvm?
    pub rw_type: MsrRWType,
    /// Handlers should be used to handling MSR.
    pub action: MsrAction,
    /// MSR source CPU.
    pub from: MsrValueFrom,
    /// Whether to override KVM MSR emulation.
    pub filter: MsrFilter,
}

#[sorted]
#[derive(Error, Debug)]
pub enum MsrExitHandlerError {
    #[error("Fail to create MSR handler")]
    HandlerCreateFailed,
}
