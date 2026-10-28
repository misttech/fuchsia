// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The `<platform.h>` entry points for the generic RISC-V 64 platform: early
//! init and the persistent RAM carve-up, CPU topology and secondary CPU
//! bring-up, the resource ranges, and the panic and halt paths.

use crate::arch_rs::riscv64::arch::{arch_disable_ints, arch_yield};
use crate::arch_rs::riscv64::mp::{
    arch_cpu_num_to_hart_id, arch_curr_cpu_num, arch_max_num_cpus, arch_mp_cpu_unplug,
    arch_mp_prep_cpu_unplug, arch_mp_send_ipi, arch_set_num_cpus, riscv64_boot_hart_id,
    riscv64_curr_hart_id, riscv64_start_cpu,
};
use crate::arch_rs::riscv64::sbi::sbi_get_cpu_state_impl;
#[cfg(console_enabled)]
use crate::console::panic_shell_start;
use crate::kernel::mp::{MpIpi, MpIpiTarget, SMP_MAX_CPUS};
use crate::kernel::types::cpu_num_t;
use crate::object::ResourceDispatcher;
use crate::pdev_interrupt::{interrupt_get_base_vector, interrupt_get_max_vector};
use crate::pdev_power::{
    PowerCpuState, PowerRebootFlags, power_cpu_off, power_get_cpu_state, power_reboot,
    power_shutdown,
};
use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use crate::platform_rs::power::{PlatformHaltAction, ZirconCrashReason};
use crate::{debuglog_rs as debuglog, persistent_debuglog_rs as persistent_debuglog, topology};
use boot_options::BootOptions;
use core::ffi::CStr;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use debug::{dprintf, tracef};
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
#[cfg(ktest)]
use unittest as _;
use zx_status::Status;
use zx_types::{ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_COUNT};

const LOCAL_TRACE: u32 = 0;

// Enable feature to probe for parked cpu cores via SBI to build
// a fallback topology tree in case one was not passed in from
// the bootloader.
// TODO(https://fxbug.dev/42079665): Remove this hack once boot shim detects cpus via device tree.
const ENABLE_SBI_TOPOLOGY_DETECT_FALLBACK: bool = true;

// The build's persistent RAM configuration, as C++ sees it through
// <kernel/persistent_ram.h>, <lib/crashlog.h>, <lib/persistent-debuglog.h>
// and <kernel/jtrace_config.h>.  The static assertions are theirs.
const PERSISTENT_RAM_ALLOCATION_GRANULARITY: usize =
    zr::parse_usize(env!("PERSISTENT_RAM_ALLOCATION_GRANULARITY"))
        .expect("PERSISTENT_RAM_ALLOCATION_GRANULARITY invalid");
// The allocation granularity of persistent RAM must be a power of two greater than 0.
zr::static_assert!(PERSISTENT_RAM_ALLOCATION_GRANULARITY.is_power_of_two());

const MIN_CRASHLOG_SIZE: usize =
    zr::parse_usize(env!("MIN_CRASHLOG_SIZE")).expect("MIN_CRASHLOG_SIZE invalid");
// Minimum reserved crashlog size must be a multiple of the persistent RAM allocation granularity.
zr::static_assert!(MIN_CRASHLOG_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY));

const TARGET_PERSISTENT_DEBUGLOG_SIZE: usize =
    zr::parse_usize(env!("TARGET_PERSISTENT_DEBUGLOG_SIZE"))
        .expect("TARGET_PERSISTENT_DEBUGLOG_SIZE invalid");
// The persistent debuglog target size must be a multiple of the persistent RAM allocation
// granularity.
zr::static_assert!(
    TARGET_PERSISTENT_DEBUGLOG_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY)
);
zr::static_assert!(TARGET_PERSISTENT_DEBUGLOG_SIZE <= u32::MAX as usize);

const JTRACE_TARGET_BUFFER_SIZE: usize =
    zr::parse_usize(env!("JTRACE_TARGET_BUFFER_SIZE")).expect("JTRACE_TARGET_BUFFER_SIZE invalid");
const JTRACE_IS_PERSISTENT: bool = cfg!(jtrace_persistent);
const JTRACE_TARGET_PERSISTENT_BUFFER_SIZE: usize =
    if JTRACE_IS_PERSISTENT { JTRACE_TARGET_BUFFER_SIZE } else { 0 };
// A persistent jtrace buffer must be a multiple of the persistent RAM allocation granularity.
zr::static_assert!(
    !JTRACE_IS_PERSISTENT
        || JTRACE_TARGET_BUFFER_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY)
);

/// `memalloc::Range` as handed off by physboot.  Only ever passed through to the
/// PMM, never read here.
#[repr(C)]
struct MemallocRange {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn cpp_jtrace_dump_current();
    fn cpp_jtrace_set_location(ptr: *mut u8, len: usize);
    fn cpp_phys_handoff_cpu_topology(count: &mut usize) -> *const zbi::TopologyNode;
    fn cpp_phys_handoff_memory(count: &mut usize) -> *const MemallocRange;
    fn cpp_phys_handoff_nvram(size: &mut usize) -> *mut u8;
    fn cpp_phys_handoff_platform_id(out: &mut zbi::PlatformId) -> bool;
    fn cpp_pmm_checker_init_from_cmdline();
    fn cpp_pmm_init(ranges: *const MemallocRange, count: usize) -> Result<(), Status>;
    fn cpp_print_current_thread_backtrace();
    fn lk_init_secondary_cpus(secondary_cpu_count: u32);
}

/// The console is compiled out, so the panic shell is a no-op like the
/// `<lib/console.h>` stub.
#[cfg(not(console_enabled))]
fn panic_shell_start() {}

static PANIC_STARTED: AtomicBool = AtomicBool::new(false);
static HALTED: AtomicBool = AtomicBool::new(false);

/// The platform's crashlog implementation, backed by the persistent RAM range
/// that the Rust platform carves out for it.
static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();

/// Whether EFI services are expected on this platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_is_efi_expected() -> bool {
    false
}

fn halt_other_cpus() {
    if !HALTED.swap(true, Ordering::SeqCst) {
        // stop the other cpus
        kprint!("stopping other cpus\n");
        arch_mp_send_ipi(MpIpiTarget::AllButLocal, 0, MpIpi::Halt);

        // spin for a while
        // TODO: find a better way to spin at this low level
        for _ in 0..100_000_000 {
            arch_yield();
        }
    }
}

// TODO(https://fxbug.dev/42180675): Refactor platform_panic_start.
/// Informs the system that a panic message is about to be printed and that
/// `platform_halt` will be called shortly.  `halt_others` says whether the
/// other CPUs are stopped first.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_panic_start(halt_others: bool) {
    arch_disable_ints();
    debuglog::dlog_panic_start();

    if halt_others {
        halt_other_cpus();
    }

    if !PANIC_STARTED.swap(true, Ordering::SeqCst) {
        debuglog::dlog_bluescreen_init();
        // Attempt to dump the current debug trace buffer, if we have one.
        // SAFETY: takes no arguments; jtrace owns the buffer it dumps.
        unsafe { cpp_jtrace_dump_current() };
    }
}

/// Stops the calling CPU.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_halt_cpu() -> ! {
    let status = power_cpu_off();

    // Should not have returned
    panic!("power_cpu_off returned {:?}\n", status);
}

/// Whether this platform supports `platform_suspend_cpu`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_supports_suspend_cpu() -> bool {
    false
}

/// Suspending a CPU is not supported on this platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_cpu(_allow_domain_power_down: bool) -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Whether this platform supports `platform_suspend_system`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_supports_suspend_system() -> bool {
    false
}

/// Suspending the system is not supported on this platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_system() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Queries the power state of `cpu_id` from the SEE, into `out_state`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_get_cpu_state(
    cpu_id: cpu_num_t,
    out_state: &mut PowerCpuState,
) -> Result<(), Status> {
    debug_assert!((cpu_id as usize) < SMP_MAX_CPUS);
    *out_state = power_get_cpu_state(arch_cpu_num_to_hart_id(cpu_id) as u64)?;
    Ok(())
}

/// The processors of the system topology, each with its riscv64 information.
/// Panics like the C++ if a processor node is anything else.
fn processors() -> impl Iterator<Item = (zbi::TopologyProcessor, zbi::TopologyRiscv64Info)> {
    topology::get_system_topology().processors().iter().map(|node| {
        let zbi::TopologyEntity::Processor(processor) = *node.entity() else {
            panic!("Invalid processor node.");
        };
        let zbi::TopologyArchitectureInfo::Riscv64(info) = processor.architecture_info else {
            panic!("Invalid processor node.");
        };
        (processor, info)
    })
}

fn topology_cpu_init() {
    debug_assert!(arch_max_num_cpus() > 0);
    // SAFETY: takes a plain count; the init subsystem owns the state it sets up.
    unsafe { lk_init_secondary_cpus(arch_max_num_cpus() - 1) };

    for (processor, info) in processors() {
        for i in 0..usize::from(processor.logical_id_count) {
            let hart_id = info.hart_id;
            debug_assert!(hart_id <= u64::from(u32::MAX));

            // Skip the current (boot) hart, we are only starting secondary harts.
            if processor.flags == zbi::TopologyProcessorFlags::PRIMARY
                || hart_id == u64::from(riscv64_boot_hart_id())
            {
                continue;
            }

            // Try to start the hart.
            let _ = riscv64_start_cpu(cpu_num_t::from(processor.logical_ids[i]), hart_id as u32);
        }
    }
}

static FALLBACK_TOPOLOGY: zbi::TopologyNode = zbi::TopologyNode {
    entity: zbi::TopologyEntity::Processor(zbi::TopologyProcessor {
        architecture_info: zbi::TopologyArchitectureInfo::Riscv64(zbi::TopologyRiscv64Info {
            hart_id: 0,
            isa_strtab_index: 0,
            reserved: 0,
        }),
        flags: zbi::TopologyProcessorFlags::PRIMARY,
        logical_ids: [0, 0, 0, 0],
        logical_id_count: 1,
    }),
    parent_index: zbi::TOPOLOGY_NO_PARENT,
};

/// Probes the first `SMP_MAX_CPUS` hart IDs through SBI and builds a flat
/// topology of the boot hart plus every stopped hart found, `max_cpus` at most.
fn sbi_detect_topology(max_cpus: usize) -> Result<fbl::Vector<zbi::TopologyNode>, Status> {
    debug_assert!(max_cpus > 0 && max_cpus <= SMP_MAX_CPUS);

    let mut detected_harts = [0u64; SMP_MAX_CPUS];

    // record the first known hart, that we're by definition running on
    detected_harts[0] = u64::from(riscv64_curr_hart_id());
    let mut detected_hart_count = 1;

    debug_assert!(arch_curr_cpu_num() == 0);

    dprintf!(INFO, "RISCV: probing for stopped harts\n");

    // probe the first SMP_MAX_CPUS harts and see which ones are present according to SBI
    // NOTE: assumes that harts are basically 0 numbered, which will not be the case always.
    // This may also detect harts that we're not supposed to run on, such as machine mode only
    // harts intended for embedded use.
    for i in 0..SMP_MAX_CPUS as u64 {
        // Stop if we've detected the clamped max cpus, including the boot cpu
        if detected_hart_count == max_cpus {
            break;
        }

        // skip the current cpu, it's known to be present
        if i == u64::from(riscv64_curr_hart_id()) {
            continue;
        }

        let Ok(state) = sbi_get_cpu_state_impl(i) else {
            continue;
        };

        if state == PowerCpuState::Stopped as u32 {
            // this is a core that exists but is stopped, add it to the list
            detected_harts[detected_hart_count] = i;
            detected_hart_count += 1;
            dprintf!(INFO, "RISCV: detected stopped hart {}\n", i);
        }
    }

    // Construct a flat topology tree based on what was found
    fbl::Vector::try_from_iter(detected_harts[..detected_hart_count].iter().enumerate().map(
        |(i, &hart_id)| zbi::TopologyNode {
            entity: zbi::TopologyEntity::Processor(zbi::TopologyProcessor {
                architecture_info: zbi::TopologyArchitectureInfo::Riscv64(
                    zbi::TopologyRiscv64Info { hart_id, isa_strtab_index: 0, reserved: 0 },
                ),
                flags: if i == 0 {
                    zbi::TopologyProcessorFlags::PRIMARY
                } else {
                    zbi::TopologyProcessorFlags::empty()
                },
                logical_ids: [i as u16, 0, 0, 0],
                logical_id_count: 1,
            }),
            parent_index: zbi::TOPOLOGY_NO_PARENT,
        },
    ))
    .map_err(|_| Status::NO_MEMORY)
}

/// Whether physboot identified the board as QEMU's riscv64 virt machine.
fn is_qemu_platform() -> bool {
    let mut platform_id = zbi::PlatformId { vid: 0, pid: 0, board_name: [0; 32] };
    // SAFETY: `platform_id` is a live local that the callee fills in when it
    // returns true.
    if !unsafe { cpp_phys_handoff_platform_id(&mut platform_id) } {
        return false;
    }
    CStr::from_bytes_until_nul(&platform_id.board_name).is_ok_and(|name| name == c"qemu-riscv64")
}

fn init_topology(_level: init::LkInitLevel) {
    let mut handoff_count = 0usize;
    // SAFETY: `handoff_count` is a live local that the callee fills in.
    let handoff_ptr = unsafe { cpp_phys_handoff_cpu_topology(&mut handoff_count) };
    let handoff: &[zbi::TopologyNode] = if handoff_ptr.is_null() || handoff_count == 0 {
        &[]
    } else {
        // SAFETY: the check above keeps an empty span's null pointer out;
        // otherwise `handoff_ptr` and `handoff_count` describe the topology
        // nodes physboot handed off, which stay valid until the handoff ends,
        // after this hook.
        unsafe { core::slice::from_raw_parts(handoff_ptr, handoff_count) }
    };

    // Read the max cpu count from the command line and clamp it to reasonable values.
    let mut max_cpus = BootOptions::get().smp_max_cpus as usize;
    if max_cpus != SMP_MAX_CPUS {
        dprintf!(INFO, "SMP: command line setting maximum cpus to {}\n", max_cpus);
    }
    if max_cpus > SMP_MAX_CPUS || max_cpus == 0 {
        kprint!(
            "SMP: invalid kernel.smp.maxcpus value ({}), clamping to {}\n",
            max_cpus,
            SMP_MAX_CPUS
        );
        max_cpus = SMP_MAX_CPUS;
    }

    // TODO-rvbringup: clamp the topology tree passed from the bootloader to max_cpus.

    // Try to initialize the system topology from a tree passed from the bootloader.
    let mut result = topology::Graph::initialize_system_topology(handoff);
    if result.is_err() {
        // Only attempt to use the SBI fallback if our global allow define is set and we're
        // running on QEMU.
        if ENABLE_SBI_TOPOLOGY_DETECT_FALLBACK && is_qemu_platform() {
            kprint!(
                "SMP: Failed to initialize system topolgy from handoff data, probing for secondary cpus via SBI\n"
            );

            // Use SBI to try to detect secondary cpus.
            match sbi_detect_topology(max_cpus) {
                Ok(topo) => {
                    // Assume the synthesized topology tree only contains processor nodes and thus
                    // the size of the array is the total detected cpu count.
                    let detected_hart_count = topo.len();
                    debug_assert!(detected_hart_count > 0 && detected_hart_count <= max_cpus);

                    // Set the detected topology.
                    result = topology::Graph::initialize_system_topology(&topo);
                    assert!(result.is_ok());
                }
                Err(err) => result = Err(err),
            }
        }
    }

    if let Err(err) = result {
        kprint!(
            "SMP: Failed to initialize system topology, error: {}, using fallback topology\n",
            err.into_raw()
        );

        // Try to fallback to a topology of just this processor.
        result =
            topology::Graph::initialize_system_topology(core::slice::from_ref(&FALLBACK_TOPOLOGY));
        assert!(result.is_ok());
    }

    let processor_count = topology::get_system_topology().processor_count();
    arch_set_num_cpus(processor_count as u32);

    // Print the detected cpu topology.
    if debug::dprintf::dprintf_enabled(debug::dprintf::INFO) {
        for (cpu_num, (_, info)) in processors().enumerate() {
            dprintf!(
                INFO,
                "System topology: CPU {} Hart {}{}\n",
                cpu_num,
                info.hart_id,
                if info.hart_id == u64::from(riscv64_curr_hart_id()) { " boot" } else { "" }
            );
        }
    }
}

init::lk_init_hook!(init_topology, init_topology, init::LK_INIT_LEVEL_VM);

/// The persistent RAM budget: the allocation granularity, the crashlog's
/// minimum, and the persistent debuglog's and debug trace's targets.
#[derive(Debug, Clone, Copy)]
struct PersistentRamBudget {
    granularity: usize,
    min_crashlog_size: usize,
    target_pdlog_size: usize,
    target_jtrace_size: usize,
}

/// The budget this kernel was built with.
const PERSISTENT_RAM_BUDGET: PersistentRamBudget = PersistentRamBudget {
    granularity: PERSISTENT_RAM_ALLOCATION_GRANULARITY,
    min_crashlog_size: MIN_CRASHLOG_SIZE,
    target_pdlog_size: TARGET_PERSISTENT_DEBUGLOG_SIZE,
    target_jtrace_size: JTRACE_TARGET_PERSISTENT_BUFFER_SIZE,
};

/// How a persistent RAM range is divided between its three users, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistentRamPartitions {
    crashlog_size: usize,
    pdlog_size: usize,
    jtrace_size: usize,
}

/// Divides `total_bytes` of persistent RAM according to `budget`.
///
/// Persistent debug logging and tracing have target amounts of RAM they would
/// _like_ to have, and crash-logging has a minimum amount it is guaranteed to
/// get.  Additionally, all allocated are made in a chunks of the minimum
/// persistent RAM allocation granularity.
///
/// Make sure that the crashlog gets as much of its minimum allocation as is
/// possible.  Then attempt to satisfy the target for persistent debug logging,
/// followed by persistent debug tracing.  Finally, give anything leftovers to
/// the crashlog.
fn partition_persistent_ram(
    total_bytes: usize,
    has_non_trivial_crashlog: bool,
    budget: &PersistentRamBudget,
) -> PersistentRamPartitions {
    // start by figuring out how many chunks of RAM we have available to
    // us total.
    let mut persistent_chunks_available = total_bytes / budget.granularity;

    // If we have not already configured a non-trivial crashlog implementation
    // for the platform, make sure that crashlog gets its minimum allocation, or
    // all of the RAM if it cannot meet even its minimum allocation.
    let mut crashlog_chunks = if !has_non_trivial_crashlog {
        core::cmp::min(persistent_chunks_available, budget.min_crashlog_size / budget.granularity)
    } else {
        0
    };
    persistent_chunks_available -= crashlog_chunks;

    // Next in line is persistent debug logging.
    let pdlog_chunks =
        core::cmp::min(persistent_chunks_available, budget.target_pdlog_size / budget.granularity);
    persistent_chunks_available -= pdlog_chunks;

    // Next up is persistent debug tracing.
    let jtrace_chunks =
        core::cmp::min(persistent_chunks_available, budget.target_jtrace_size / budget.granularity);
    persistent_chunks_available -= jtrace_chunks;

    // Finally, anything left over can go to the crashlog.
    crashlog_chunks += persistent_chunks_available;

    PersistentRamPartitions {
        crashlog_size: crashlog_chunks * budget.granularity,
        pdlog_size: pdlog_chunks * budget.granularity,
        jtrace_size: jtrace_chunks * budget.granularity,
    }
}

fn allocate_persistent_ram(range: &'static mut [u8]) {
    // Figure out how to divide up our persistent RAM.  Right now there are
    // three potential users:
    //
    // 1) The crashlog.
    // 2) Persistent debug logging.
    // 3) Persistent debug tracing.
    //
    let has_non_trivial_crashlog = PlatformCrashlog::has_non_trivial_impl();
    let PersistentRamPartitions { crashlog_size, pdlog_size, jtrace_size } =
        partition_persistent_ram(range.len(), has_non_trivial_crashlog, &PERSISTENT_RAM_BUDGET);

    // Configure up the crashlog RAM
    let (crashlog, leftover) = range.split_at_mut(crashlog_size);
    if crashlog_size > 0 {
        dprintf!(INFO, "Crashlog configured with {} bytes\n", crashlog_size);
        // SAFETY: `allocate_persistent_ram` runs at most once during single-threaded early boot
        // before `MAPPED_CRASHLOG` is read via `PlatformCrashlog::bind`.
        unsafe {
            let _ = Pin::static_ref(&MAPPED_CRASHLOG).init_pin(MappedCrashlog::new(crashlog));
            let _ = PlatformCrashlog::bind(MAPPED_CRASHLOG.get_unchecked());
        }
    }

    // Configure the persistent debuglog RAM (if we have any)
    let (pdlog, leftover) = leftover.split_at_mut(pdlog_size);
    if pdlog_size > 0 {
        dprintf!(
            INFO,
            "Persistent debug logging enabled and configured with {} bytes\n",
            pdlog_size
        );
        persistent_debuglog::persistent_dlog_set_location(pdlog);
    }

    // Do _not_ attempt to set the location of the debug trace buffer if this is
    // not a persistent debug trace buffer.  The location of a non-persistent
    // trace buffer would have been already set during (very) early init.
    if JTRACE_IS_PERSISTENT {
        let (jtrace, _) = leftover.split_at_mut(jtrace_size);
        // SAFETY: `jtrace` is the tail of the persistent RAM range, which stays
        // mapped for the life of the kernel; jtrace keeps using it and nothing
        // here touches it again.
        unsafe { cpp_jtrace_set_location(jtrace.as_mut_ptr(), jtrace.len()) };
    }
}

/// Early platform initialization: claims the persistent RAM and brings up the
/// PMM from the handoff memory map.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_early_init() {
    let mut nvram_size = 0usize;
    // SAFETY: `nvram_size` is a live local that the callee fills in.
    let nvram = unsafe { cpp_phys_handoff_nvram(&mut nvram_size) };
    if nvram_size != 0 {
        // SAFETY: physboot hands off `nvram_size` bytes of persistent RAM at
        // `nvram`, mapped and wired for the life of the kernel, and this is the
        // only place that ever takes a reference to them.
        let range = unsafe { core::slice::from_raw_parts_mut(nvram, nvram_size) };
        allocate_persistent_ram(range);
    }

    // Initialize the PmmChecker now that the cmdline has been parsed.
    // SAFETY: takes no arguments; the PMM checker owns the state it initializes.
    unsafe { cpp_pmm_checker_init_from_cmdline() };

    let mut range_count = 0usize;
    // SAFETY: `range_count` is a live local that the callee fills in.
    let ranges = unsafe { cpp_phys_handoff_memory(&mut range_count) };
    // SAFETY: `ranges` and `range_count` describe the memory ranges physboot
    // handed off, which stay valid until the handoff ends, after this call.
    assert!(unsafe { cpp_pmm_init(ranges, range_count) }.is_ok());
}

/// Platform initialization before the VM is up.  Nothing to do here.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_prevm_init() {}

// Called after the heap is up but before the system is multithreaded.
fn platform_init_pre_thread(_level: init::LkInitLevel) {}

init::lk_init_hook!(platform_init_pre_thread, platform_init_pre_thread, init::LK_INIT_LEVEL_VM);

/// Platform initialization once threading is up: starts the secondary CPUs.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_init() {
    topology_cpu_init();
}

// after the fact create a region to reserve the peripheral map(s)
fn platform_init_postvm(_level: init::LkInitLevel) {}

init::lk_init_hook!(platform_postvm, platform_init_postvm, init::LK_INIT_LEVEL_VM);

/// Halts the system the platform's way: reboots or shuts down as
/// `suggested_action` asks, drops into the panic shell after a panic when
/// `halt_on_panic` is set, and otherwise spins forever.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_specific_halt(
    suggested_action: PlatformHaltAction,
    reason: ZirconCrashReason,
    halt_on_panic: bool,
) -> ! {
    tracef!(
        "suggested_action {}, reason {}, halt_on_panic {}\n",
        suggested_action as u32,
        reason as u32,
        halt_on_panic
    );
    match suggested_action {
        PlatformHaltAction::Reboot => {
            power_reboot(if reason == ZirconCrashReason::NoCrash {
                PowerRebootFlags::Normal
            } else {
                PowerRebootFlags::Panic
            });
            kprint!("reboot failed\n");
        }
        PlatformHaltAction::RebootBootloader => {
            power_reboot(PowerRebootFlags::Bootloader);
            kprint!("reboot-bootloader failed\n");
        }
        PlatformHaltAction::RebootRecovery => {
            power_reboot(PowerRebootFlags::Recovery);
            kprint!("reboot-recovery failed\n");
        }
        PlatformHaltAction::Shutdown => {
            power_shutdown();
            kprint!("shutdown failed\n");
        }
        PlatformHaltAction::Halt => {}
    }

    if reason == ZirconCrashReason::Panic {
        // SAFETY: takes no arguments; prints the current thread's backtrace.
        unsafe { cpp_print_current_thread_backtrace() };
        if !halt_on_panic {
            power_reboot(PowerRebootFlags::Panic);
            kprint!("reboot failed\n");
        }
        dprintf!(ALWAYS, "CRASH: starting debug shell... (reason = {})\n", reason as u32);
        arch_disable_ints();
        panic_shell_start();
    }

    dprintf!(ALWAYS, "HALT: spinning forever... (reason = {})\n", reason as u32);

    // catch all fallthrough cases
    arch_disable_ints();

    loop {
        arch_yield();
    }
}

// Initialize Resource system after the heap is initialized.
fn riscv64_resource_dispatcher_init_hook(_level: init::LkInitLevel) {
    // 64 bit address space for MMIO on RISCV64
    if let Err(status) = ResourceDispatcher::initialize_allocator(ZX_RSRC_KIND_MMIO, 0, usize::MAX)
    {
        kprint!("Resources: Failed to initialize MMIO allocator: {}\n", status.into_raw());
    }
    // Set up IRQs based on values from the PLIC
    // SAFETY: the PLIC registered the interrupt ops during the driver handoff,
    // before the init hooks of this level run.
    let (base_vector, max_vector) =
        unsafe { (*interrupt_get_base_vector(), *interrupt_get_max_vector()) };
    // Normally there would be at least one interrupt vector.
    debug_assert!(max_vector > 0);
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_IRQ,
        u64::from(base_vector),
        max_vector as usize,
    ) {
        kprint!("Resources: Failed to initialize IRQ allocator: {}\n", status.into_raw());
    }
    // Set up range of valid system resources.
    if let Err(status) = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SYSTEM,
        0,
        ZX_RSRC_SYSTEM_COUNT as usize,
    ) {
        kprint!("Resources: Failed to initialize system allocator: {}\n", status.into_raw());
    }
}

init::lk_init_hook!(
    riscv64_resource_init,
    riscv64_resource_dispatcher_init_hook,
    init::LK_INIT_LEVEL_INTC
);

/// Prepares to unplug `cpu_id`; the arch decides whether it may go.
#[unsafe(no_mangle)]
pub extern "C" fn platform_mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    arch_mp_prep_cpu_unplug(cpu_id)
}

/// Unplugs `cpu_id`; the arch does the work.
#[unsafe(no_mangle)]
pub extern "C" fn platform_mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    arch_mp_cpu_unplug(cpu_id)
}

/// Tests for the generic RISC-V 64 platform.
#[cfg(ktest)]
#[unittest::suite(name = "generic_riscv64_platform")]
mod tests {
    use super::{PersistentRamBudget, partition_persistent_ram};
    use unittest::assert_eq;

    const BUDGET: PersistentRamBudget = PersistentRamBudget {
        granularity: 128,
        min_crashlog_size: 2048,
        target_pdlog_size: 8192,
        target_jtrace_size: 8192,
    };

    /// Chunks go to the crashlog minimum, the two targets, then back to the crashlog.
    #[test]
    fn test_partition_persistent_ram() {
        // Enough for everybody: the two targets are met and the crashlog gets
        // its minimum plus everything left over.
        let partitions = partition_persistent_ram(65536, false, &BUDGET);
        assert_eq!(partitions.crashlog_size, 49152);
        assert_eq!(partitions.pdlog_size, 8192);
        assert_eq!(partitions.jtrace_size, 8192);

        // A non-trivial crashlog is already bound, so it takes no minimum.
        let partitions = partition_persistent_ram(32768, true, &BUDGET);
        assert_eq!(partitions.crashlog_size, 16384);
        assert_eq!(partitions.pdlog_size, 8192);
        assert_eq!(partitions.jtrace_size, 8192);

        // Less than the crashlog minimum: the crashlog takes all of it.
        let partitions = partition_persistent_ram(1024, false, &BUDGET);
        assert_eq!(partitions.crashlog_size, 1024);
        assert_eq!(partitions.pdlog_size, 0);
        assert_eq!(partitions.jtrace_size, 0);

        // Sizes round down to whole chunks.
        let partitions = partition_persistent_ram(1000, false, &BUDGET);
        assert_eq!(partitions.crashlog_size, 896);
        assert_eq!(partitions.pdlog_size, 0);
        assert_eq!(partitions.jtrace_size, 0);

        // Nothing at all.
        let partitions = partition_persistent_ram(0, false, &BUDGET);
        assert_eq!(partitions.crashlog_size, 0);
        assert_eq!(partitions.pdlog_size, 0);
        assert_eq!(partitions.jtrace_size, 0);
    }
}
