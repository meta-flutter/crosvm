// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::arch::x86_64::{CpuidResult, __cpuid, __cpuid_count};
use std::cmp;
use std::result;

use devices::{Apic, IrqChipCap, IrqChipX86_64};
use hypervisor::{CpuIdEntry, HypervisorCap, HypervisorX86_64, VcpuX86_64};

use crate::CpuManufacturer;
use remain::sorted;
use thiserror::Error;

#[sorted]
#[derive(Error, Debug, PartialEq)]
pub enum Error {
    #[error("GetSupportedCpus ioctl failed: {0}")]
    GetSupportedCpusFailed(base::Error),
    #[error("SetSupportedCpus ioctl failed: {0}")]
    SetSupportedCpusFailed(base::Error),
}

pub type Result<T> = result::Result<T, Error>;

// CPUID bits in ebx, ecx, and edx.
const EBX_CLFLUSH_CACHELINE: u32 = 8; // Flush a cache line size.
const EBX_CLFLUSH_SIZE_SHIFT: u32 = 8; // Bytes flushed when executing CLFLUSH.
const EBX_CPU_COUNT_SHIFT: u32 = 16; // Index of this CPU.
const EBX_CPUID_SHIFT: u32 = 24; // Index of this CPU.
const ECX_EPB_SHIFT: u32 = 3; // "Energy Performance Bias" bit.
const ECX_X2APIC_SHIFT: u32 = 21; // APIC supports extended xAPIC (x2APIC) standard.
const ECX_TSC_DEADLINE_TIMER_SHIFT: u32 = 24; // TSC deadline mode of APIC timer.
const ECX_HYPERVISOR_SHIFT: u32 = 31; // Flag to be set when the cpu is running on a hypervisor.
const EDX_HTT_SHIFT: u32 = 28; // Hyper Threading Enabled.
const ECX_TOPO_TYPE_SHIFT: u32 = 8; // Topology Level type.
const ECX_TOPO_SMT_TYPE: u32 = 1; // SMT type.
const ECX_TOPO_CORE_TYPE: u32 = 2; // CORE type.
const ECX_HCFC_PERF_SHIFT: u32 = 0; // Presence of IA32_MPERF and IA32_APERF.
const EAX_CPU_CORES_SHIFT: u32 = 26; // Index of cpu cores in the same physical package.
const EDX_HYBRID_CPU_SHIFT: u32 = 15; // Hybrid. The processor is identified as a hybrid part.
const EAX_HWP_SHIFT: u32 = 7; // Intel Hardware P-states.
const EAX_HWP_EPP_SHIFT: u32 = 10; // HWP Energy Perf. Preference.
const EAX_ITMT_SHIFT: u32 = 14; // Intel Turbo Boost Max Technology 3.0 available.
const EAX_CORE_TEMP: u32 = 0; // Core Temperature
const EAX_PKG_TEMP: u32 = 6; // Package Temperature

/// All of the context required to emulate the CPUID instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuIdContext {
    /// Id of the Vcpu associated with this context.
    vcpu_id: usize,
    /// The total number of vcpus on this VM.
    cpu_count: usize,
    /// Whether or not SMT should be disabled.
    no_smt: bool,
    /// Whether or not the IrqChip's APICs support X2APIC.
    x2apic: bool,
    /// Whether or not the IrqChip's APICs support a TSC deadline timer.
    tsc_deadline_timer: bool,
    /// The frequency at which the IrqChip's APICs run.
    apic_frequency: u32,
    /// The TSC frequency in Hz, if it could be determined.
    tsc_frequency: Option<u64>,
    /// Whether to force the use of a calibrated TSC cpuid leaf (0x15) even if
    /// the hypervisor doesn't require it.
    force_calibrated_tsc_leaf: bool,
    /// Whether the hypervisor requires a calibrated TSC cpuid leaf (0x15).
    calibrated_tsc_leaf_required: bool,
    /// Whether or not VCPU IDs and APIC IDs should match host cpu IDs.
    host_cpu_topology: bool,
    enable_pnp_data: bool,
    /// Enable Intel Turbo Boost Max Technology 3.0.
    itmt: bool,
    /// __cpuid_count or a fake function for test.
    cpuid_count: unsafe fn(u32, u32) -> CpuidResult,
    /// __cpuid or a fake function for test.
    cpuid: unsafe fn(u32) -> CpuidResult,
}

impl CpuIdContext {
    pub fn new(
        vcpu_id: usize,
        cpu_count: usize,
        no_smt: bool,
        host_cpu_topology: bool,
        irq_chip: Option<&dyn IrqChipX86_64>,
        enable_pnp_data: bool,
        itmt: bool,
        force_calibrated_tsc_leaf: bool,
        calibrated_tsc_leaf_required: bool,
        cpuid_count: unsafe fn(u32, u32) -> CpuidResult,
        cpuid: unsafe fn(u32) -> CpuidResult,
    ) -> CpuIdContext {
        CpuIdContext {
            vcpu_id,
            cpu_count,
            no_smt,
            x2apic: irq_chip.map_or(false, |chip| chip.check_capability(IrqChipCap::X2Apic)),
            tsc_deadline_timer: irq_chip.map_or(false, |chip| {
                chip.check_capability(IrqChipCap::TscDeadlineTimer)
            }),
            apic_frequency: irq_chip.map_or(Apic::frequency(), |chip| chip.lapic_frequency()),
            tsc_frequency: devices::tsc::tsc_frequency().ok(),
            force_calibrated_tsc_leaf,
            calibrated_tsc_leaf_required,
            host_cpu_topology,
            enable_pnp_data,
            itmt,
            cpuid_count,
            cpuid,
        }
    }
}

/// Adjust a CPUID instruction result to return values that work with crosvm.
///
/// Given an input CpuIdEntry `entry`, which represents what the Hypervisor would normally return
/// for a given CPUID instruction result, adjust that result to reflect the capabilities of crosvm.
/// The `ctx` argument contains all of the Vm-specific and Vcpu-specific information required to
/// return the appropriate results.
pub fn adjust_cpuid(entry: &mut CpuIdEntry, ctx: &CpuIdContext) {
    match entry.function {
        0 => {
            if ctx.tsc_frequency.is_some() {
                // We add leaf 0x15 for the TSC frequency if it is available.
                entry.cpuid.eax = cmp::max(0x15, entry.cpuid.eax);
            }
        }
        1 => {
            // X86 hypervisor feature
            if entry.index == 0 {
                entry.cpuid.ecx |= 1 << ECX_HYPERVISOR_SHIFT;
            }
            if ctx.x2apic {
                entry.cpuid.ecx |= 1 << ECX_X2APIC_SHIFT;
            } else {
                entry.cpuid.ecx &= !(1 << ECX_X2APIC_SHIFT);
            }
            if ctx.tsc_deadline_timer {
                entry.cpuid.ecx |= 1 << ECX_TSC_DEADLINE_TIMER_SHIFT;
            }

            if ctx.host_cpu_topology {
                entry.cpuid.ebx |= EBX_CLFLUSH_CACHELINE << EBX_CLFLUSH_SIZE_SHIFT;

                // Expose HT flag to Guest.
                let result = unsafe { (ctx.cpuid)(entry.function) };
                entry.cpuid.edx |= result.edx & (1 << EDX_HTT_SHIFT);
                return;
            }

            entry.cpuid.ebx = (ctx.vcpu_id << EBX_CPUID_SHIFT) as u32
                | (EBX_CLFLUSH_CACHELINE << EBX_CLFLUSH_SIZE_SHIFT);
            if ctx.cpu_count > 1 {
                // This field is only valid if CPUID.1.EDX.HTT[bit 28]= 1.
                entry.cpuid.ebx |= (ctx.cpu_count as u32) << EBX_CPU_COUNT_SHIFT;
                // A value of 0 for HTT indicates there is only a single logical
                // processor in the package and software should assume only a
                // single APIC ID is reserved.
                entry.cpuid.edx |= 1 << EDX_HTT_SHIFT;
            }
        }
        2 | // Cache and TLB Descriptor information
        0x80000002 | 0x80000003 | 0x80000004 | // Processor Brand String
        0x80000005 | 0x80000006 // L1 and L2 cache information
            => entry.cpuid = unsafe { (ctx.cpuid)(entry.function) },
        4 => {
            entry.cpuid = unsafe { (ctx.cpuid_count)(entry.function, entry.index) };

            if ctx.host_cpu_topology {
                return;
            }

            entry.cpuid.eax &= !0xFC000000;
            if ctx.cpu_count > 1 {
                let cpu_cores = if ctx.no_smt {
                    ctx.cpu_count as u32
                } else if ctx.cpu_count % 2 == 0 {
                    (ctx.cpu_count >> 1) as u32
                } else {
                    1
                };
                entry.cpuid.eax |= (cpu_cores - 1) << EAX_CPU_CORES_SHIFT;
            }
        }
        6 => {
            // Clear X86 EPB feature.  No frequency selection in the hypervisor.
            entry.cpuid.ecx &= !(1 << ECX_EPB_SHIFT);

            // Set ITMT related features.
            if ctx.itmt || ctx.enable_pnp_data {
                // Safe because we pass 6 for this call and the host
                // supports the `cpuid` instruction
                let result = unsafe { (ctx.cpuid)(entry.function) };
                if ctx.itmt {
                    // Expose ITMT to guest.
                    entry.cpuid.eax |= result.eax & (1 << EAX_ITMT_SHIFT);
                    // Expose HWP and HWP_EPP to guest.
                    entry.cpuid.eax |= result.eax & (1 << EAX_HWP_SHIFT);
                    entry.cpuid.eax |= result.eax & (1 << EAX_HWP_EPP_SHIFT);
                }
                if ctx.enable_pnp_data {
                    // Expose core temperature, package temperature
                    // and APEF/MPERF to guest
                    entry.cpuid.eax |= result.eax & (1 << EAX_CORE_TEMP);
                    entry.cpuid.eax |= result.eax & (1 << EAX_PKG_TEMP);
                    entry.cpuid.ecx |= result.ecx & (1 << ECX_HCFC_PERF_SHIFT);
                }
            }
        }
        7 => {
            if ctx.host_cpu_topology && entry.index == 0 {
                // Safe because we pass 7 and 0 for this call and the host supports the
                // `cpuid` instruction
                let result = unsafe { (ctx.cpuid_count)(entry.function, entry.index) };
                entry.cpuid.edx |= result.edx & (1 << EDX_HYBRID_CPU_SHIFT);
            }
        }
        0x15 => {
            if ctx.calibrated_tsc_leaf_required
                || ctx.force_calibrated_tsc_leaf {

                let cpuid_15 = ctx
                    .tsc_frequency
                    .map(|tsc_freq| devices::tsc::fake_tsc_frequency_cpuid(
                            tsc_freq, ctx.apic_frequency));

                if let Some(new_entry) = cpuid_15 {
                    entry.cpuid = new_entry.cpuid;
                }
            } else if ctx.enable_pnp_data {
                // Expose TSC frequency to guest
                // Safe because we pass 0x15 for this call and the host
                // supports the `cpuid` instruction
                entry.cpuid = unsafe { (ctx.cpuid)(entry.function) };
            }
        }
        0x1A => {
            // Hybrid information leaf.
            if ctx.host_cpu_topology {
                // Safe because we pass 0x1A for this call and the host supports the
                // `cpuid` instruction
                entry.cpuid = unsafe { (ctx.cpuid)(entry.function) };
            }
        }
        0xB | 0x1F => {
            if ctx.host_cpu_topology {
                return;
            }
            // Extended topology enumeration / V2 Extended topology enumeration
            // NOTE: these will need to be split if any of the fields that differ between
            // the two versions are to be set.
            // On AMD, these leaves are not used, so it is currently safe to leave in.
            entry.cpuid.edx = ctx.vcpu_id as u32; // x2APIC ID
            if entry.index == 0 {
                if ctx.no_smt || (ctx.cpu_count == 1) {
                    // Make it so that all VCPUs appear as different,
                    // non-hyperthreaded cores on the same package.
                    entry.cpuid.eax = 0; // Shift to get id of next level
                    entry.cpuid.ebx = 1; // Number of logical cpus at this level
                } else if ctx.cpu_count % 2 == 0 {
                    // Each core has 2 hyperthreads
                    entry.cpuid.eax = 1; // Shift to get id of next level
                    entry.cpuid.ebx = 2; // Number of logical cpus at this level
                } else {
                    // One core contain all the cpu_count hyperthreads
                    let cpu_bits: u32 = 32 - ((ctx.cpu_count - 1) as u32).leading_zeros();
                    entry.cpuid.eax = cpu_bits; // Shift to get id of next level
                    entry.cpuid.ebx = ctx.cpu_count as u32; // Number of logical cpus at this level
                }
                entry.cpuid.ecx = (ECX_TOPO_SMT_TYPE << ECX_TOPO_TYPE_SHIFT) | entry.index;
            } else if entry.index == 1 {
                let cpu_bits: u32 = 32 - ((ctx.cpu_count - 1) as u32).leading_zeros();
                entry.cpuid.eax = cpu_bits;
                // Number of logical cpus at this level
                entry.cpuid.ebx = (ctx.cpu_count as u32) & 0xffff;
                entry.cpuid.ecx = (ECX_TOPO_CORE_TYPE << ECX_TOPO_TYPE_SHIFT) | entry.index;
            } else {
                entry.cpuid.eax = 0;
                entry.cpuid.ebx = 0;
                entry.cpuid.ecx = 0;
            }
        }
        _ => (),
    }
}

/// Adjust all the entries in `cpuid` based on crosvm's cpuid logic and `ctx`. Calls `adjust_cpuid`
/// on each entry in `cpuid`, and adds any entries that should exist and are missing from `cpuid`.
fn filter_cpuid(cpuid: &mut hypervisor::CpuId, ctx: &CpuIdContext) {
    // Add an empty leaf 0x15 if we have a tsc_frequency and it's not in the current set of leaves.
    // It will be filled with the appropriate frequency information by `adjust_cpuid`.
    if ctx.tsc_frequency.is_some()
        && !cpuid
            .cpu_id_entries
            .iter()
            .any(|entry| entry.function == 0x15)
    {
        cpuid.cpu_id_entries.push(CpuIdEntry {
            function: 0x15,
            index: 0,
            flags: 0,
            cpuid: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
        })
    }

    let entries = &mut cpuid.cpu_id_entries;
    for entry in entries.iter_mut() {
        adjust_cpuid(entry, ctx);
    }
}

/// Sets up the cpuid entries for the given vcpu.  Can fail if there are too many CPUs specified or
/// if an ioctl returns an error.
///
/// # Arguments
///
/// * `hypervisor` - `HypervisorX86_64` impl for getting supported CPU IDs.
/// * `irq_chip` - `IrqChipX86_64` for adjusting appropriate IrqChip CPUID bits.
/// * `vcpu` - `VcpuX86_64` for setting CPU ID.
/// * `vcpu_id` - The vcpu index of `vcpu`.
/// * `nrcpus` - The number of vcpus being used by this VM.
/// * `no_smt` - The flag indicates whether vCPUs supports SMT.
/// * `host_cpu_topology` - The flag indicates whether vCPUs use mirror CPU topology.
/// * `enable_pnp_data` - The flag indicates whether vCPU shows PnP data.
/// * `itmt` - The flag indicates whether vCPU use ITMT scheduling feature.
pub fn setup_cpuid(
    hypervisor: &dyn HypervisorX86_64,
    irq_chip: &dyn IrqChipX86_64,
    vcpu: &dyn VcpuX86_64,
    vcpu_id: usize,
    nrcpus: usize,
    no_smt: bool,
    host_cpu_topology: bool,
    enable_pnp_data: bool,
    itmt: bool,
    force_calibrated_tsc_leaf: bool,
) -> Result<()> {
    let mut cpuid = hypervisor
        .get_supported_cpuid()
        .map_err(Error::GetSupportedCpusFailed)?;

    filter_cpuid(
        &mut cpuid,
        &CpuIdContext::new(
            vcpu_id,
            nrcpus,
            no_smt,
            host_cpu_topology,
            Some(irq_chip),
            enable_pnp_data,
            itmt,
            force_calibrated_tsc_leaf,
            hypervisor.check_capability(HypervisorCap::CalibratedTscLeafRequired),
            __cpuid_count,
            __cpuid,
        ),
    );

    vcpu.set_cpuid(&cpuid)
        .map_err(Error::SetSupportedCpusFailed)
}

const MANUFACTURER_ID_FUNCTION: u32 = 0x00000000;
const AMD_EBX: u32 = u32::from_le_bytes([b'A', b'u', b't', b'h']);
const AMD_EDX: u32 = u32::from_le_bytes([b'e', b'n', b't', b'i']);
const AMD_ECX: u32 = u32::from_le_bytes([b'c', b'A', b'M', b'D']);
const INTEL_EBX: u32 = u32::from_le_bytes([b'G', b'e', b'n', b'u']);
const INTEL_EDX: u32 = u32::from_le_bytes([b'i', b'n', b'e', b'I']);
const INTEL_ECX: u32 = u32::from_le_bytes([b'n', b't', b'e', b'l']);

pub fn cpu_manufacturer() -> CpuManufacturer {
    // safe because MANUFACTURER_ID_FUNCTION is a well known cpuid function,
    // and we own the result value afterwards.
    let result = unsafe { __cpuid(MANUFACTURER_ID_FUNCTION) };
    if result.ebx == AMD_EBX && result.edx == AMD_EDX && result.ecx == AMD_ECX {
        return CpuManufacturer::Amd;
    } else if result.ebx == INTEL_EBX && result.edx == INTEL_EDX && result.ecx == INTEL_ECX {
        return CpuManufacturer::Intel;
    }
    return CpuManufacturer::Unknown;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use hypervisor::ProtectionType;

    #[test]
    fn cpu_manufacturer_test() {
        // this should be amd or intel. We don't support other processors for virtualization.
        let manufacturer = cpu_manufacturer();
        assert_ne!(manufacturer, CpuManufacturer::Unknown);
    }

    #[test]
    #[cfg(unix)]
    fn feature_and_vendor_name() {
        let mut cpuid = hypervisor::CpuId::new(2);
        let guest_mem =
            vm_memory::GuestMemory::new(&[(vm_memory::GuestAddress(0), 0x10000)]).unwrap();
        let kvm = hypervisor::kvm::Kvm::new().unwrap();
        let vm = hypervisor::kvm::KvmVm::new(&kvm, guest_mem, ProtectionType::Unprotected).unwrap();
        let irq_chip = devices::KvmKernelIrqChip::new(vm, 1).unwrap();

        let entries = &mut cpuid.cpu_id_entries;
        entries.push(hypervisor::CpuIdEntry {
            function: 0,
            index: 0,
            flags: 0,
            cpuid: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            },
        });
        entries.push(hypervisor::CpuIdEntry {
            function: 1,
            index: 0,
            flags: 0,
            cpuid: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 0x10,
                edx: 0,
            },
        });
        filter_cpuid(
            &mut cpuid,
            &CpuIdContext::new(
                1,
                2,
                false,
                false,
                Some(&irq_chip),
                false,
                false,
                false,
                false,
                __cpuid_count,
                __cpuid,
            ),
        );

        let entries = &mut cpuid.cpu_id_entries;
        assert_eq!(entries[0].function, 0);
        assert_eq!(1, (entries[1].cpuid.ebx >> EBX_CPUID_SHIFT) & 0x000000ff);
        assert_eq!(
            2,
            (entries[1].cpuid.ebx >> EBX_CPU_COUNT_SHIFT) & 0x000000ff
        );
        assert_eq!(
            EBX_CLFLUSH_CACHELINE,
            (entries[1].cpuid.ebx >> EBX_CLFLUSH_SIZE_SHIFT) & 0x000000ff
        );
        assert_ne!(0, entries[1].cpuid.ecx & (1 << ECX_HYPERVISOR_SHIFT));
        assert_ne!(0, entries[1].cpuid.edx & (1 << EDX_HTT_SHIFT));
    }

    #[test]
    fn cpuid_copies_register() {
        let fake_cpuid_count = |_function: u32, _index: u32| CpuidResult {
            eax: 27,
            ebx: 18,
            ecx: 28,
            edx: 18,
        };
        let fake_cpuid = |_function: u32| CpuidResult {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        let ctx = CpuIdContext {
            vcpu_id: 0,
            cpu_count: 0,
            no_smt: false,
            x2apic: false,
            tsc_deadline_timer: false,
            apic_frequency: 0,
            tsc_frequency: None,
            host_cpu_topology: true,
            enable_pnp_data: false,
            itmt: false,
            force_calibrated_tsc_leaf: false,
            calibrated_tsc_leaf_required: false,
            cpuid_count: fake_cpuid_count,
            cpuid: fake_cpuid,
        };
        let mut cpu_id_entry = CpuIdEntry {
            function: 0x4,
            index: 0,
            flags: 0,
            cpuid: CpuidResult {
                eax: 31,
                ebx: 41,
                ecx: 59,
                edx: 26,
            },
        };
        adjust_cpuid(&mut cpu_id_entry, &ctx);
        assert_eq!(cpu_id_entry.cpuid.eax, 27)
    }
}
