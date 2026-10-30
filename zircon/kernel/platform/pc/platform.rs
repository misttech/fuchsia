// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2015 Intel Corporation
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::x86::lapic::{apic_bsp_id, apic_local_id};
use crate::arch_rs::x86::pv::PvEoi;
use crate::arch_rs::x86::smp::{x86_bringup_aps, x86_init_smp};
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::types::cpu_num_t;
use crate::pdev_power::PowerCpuState;
use crate::platform_rs::crashlog::PlatformCrashlog;
use crate::platform_rs::efi_crashlog::EfiCrashlog;
use crate::platform_rs::mapped_crashlog::MappedCrashlog;
use crate::topology::{self, Graph};
use boot_options::BootOptions;
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_void};
use core::pin::Pin;
use core::ptr::slice_from_raw_parts;
use core::sync::atomic::Ordering;
use debug::dprintf;
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
use zerocopy::FromBytes;
use zx_status::Status;

static MAPPED_CRASHLOG: LazyInit<MappedCrashlog, NoCheck> = LazyInit::uninit();
static EFI_CRASHLOG: EfiCrashlog = EfiCrashlog::new();

type CpuDistanceCallback =
    extern "C" fn(from_id: cpu_num_t, to_id: cpu_num_t, ctx: *mut c_void) -> u32;

unsafe extern "C" {
    fn arch_mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status>;
    fn arch_mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status>;

    fn cpp_phys_handoff_crashlog(size: &mut usize) -> *const u8;
    fn cpp_phys_handoff_nvram(size: &mut usize) -> *mut u8;
    fn cpp_phys_handoff_memory(count: &mut usize) -> *const memalloc::Range;
    fn cpp_cpu_distance_map_init(cpu_count: usize, cb: CpuDistanceCallback, ctx: *mut c_void);
    fn cpp_cpu_distance_map_set_distance_threshold(threshold: u32);
    fn cpp_cpu_distance_map_dump();
}

/// Preserves crashlog and NVRAM locations handed off from physboot before handoff memory is
/// reclaimed.
fn platform_save_bootloader_data() {
    // Record any previous crashlog.
    let mut crashlog_size = 0usize;
    // SAFETY: `crashlog_size` is a live local that the callee fills in.
    let crashlog_ptr = unsafe { cpp_phys_handoff_crashlog(&mut crashlog_size) };
    if !crashlog_ptr.is_null() && crashlog_size != 0 {
        // SAFETY: `crashlog_ptr` and `crashlog_size` describe the crashlog byte slice handed off by
        // physboot, which remains valid for all subsequent calls to `recover`.
        unsafe {
            EFI_CRASHLOG
                .set_last_crashlog_location(slice_from_raw_parts(crashlog_ptr, crashlog_size));
        }
    }

    // If we have an NVRAM location and we have not already configured a platform
    // crashlog implementation, use the NVRAM location to back a
    // MappedCrashlog implementation and configure the generic platform
    // layer to use it.
    let mut nvram_size = 0usize;
    // SAFETY: `nvram_size` is a live local that the callee fills in.
    let nvram_ptr = unsafe { cpp_phys_handoff_nvram(&mut nvram_size) };
    if nvram_size != 0 && !PlatformCrashlog::has_non_trivial_impl() {
        // SAFETY: `nvram_ptr` and `nvram_size` describe the 8-byte aligned persistent RAM range
        // handed off by physboot, which stays mapped and exclusively owned by `MAPPED_CRASHLOG`
        // for the life of the kernel, and `platform_save_bootloader_data` runs at most once during
        // single-threaded early boot.
        unsafe {
            let range = zr::slice_from_raw_parts_mut(nvram_ptr, nvram_size);
            let _ = Pin::static_ref(&MAPPED_CRASHLOG).init_pin(MappedCrashlog::new(range));
            let _ = PlatformCrashlog::bind(MAPPED_CRASHLOG.get_unchecked());
        }
    }
}

/// Selects the default [`EfiCrashlog`] implementation if no crashlog implementation was bound
/// early.
fn platform_init_crashlog() {
    // Nothing to do if we have already selected a crashlog implementation.
    if PlatformCrashlog::has_non_trivial_impl() {
        return;
    }

    // Initialize and select the EfiCrashlog implementation.
    let _ = PlatformCrashlog::bind(&EFI_CRASHLOG);
}

/// Super early platform initialization, before almost everything.
fn platform_early_init() {
    /* extract bootloader data while still accessible */
    /* this includes debug uart config, etc. */
    platform_save_bootloader_data();

    /* initialize physical memory arenas */
    let mut range_count = 0usize;
    // SAFETY: `range_count` is a live local that the callee fills in.
    let ranges_ptr = unsafe { cpp_phys_handoff_memory(&mut range_count) };
    // SAFETY: `ranges_ptr` and `range_count` describe the memory ranges handed off by physboot,
    // and `platform_early_init` is called once during single-threaded early boot.
    unsafe {
        let ranges = zr::slice_from_raw_parts(ranges_ptr, range_count);
        super::memory::pc_mem_init(ranges);
    }
}

/// FFI entry point for [`platform_early_init`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_early_init() {
    platform_early_init();
}

/// Perform any set up required before virtual memory is enabled, or the heap is set up.
fn platform_prevm_init() {}

/// FFI entry point for [`platform_prevm_init`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_prevm_init() {
    platform_prevm_init();
}

/// SMP topology discovery state populated during [`traverse_topology`] and consumed by
/// [`platform_init_smp`] on the bootstrap processor.
struct SmpBootData {
    /// Maps from contiguous CPU ID to APIC ID.
    apic_ids: fbl::Vector<u32>,
    /// Index of the bootstrap processor within `apic_ids`.
    bsp_apic_id_index: usize,
}

impl SmpBootData {
    const fn new() -> Self {
        Self { apic_ids: fbl::Vector::new(), bsp_apic_id_index: 0 }
    }
}

/// Wrapper for [`SmpBootData`] accessed only on the bootstrap processor during single-CPU boot
/// initialization.
struct BootCell(UnsafeCell<SmpBootData>);

impl BootCell {
    const fn new() -> Self {
        Self(UnsafeCell::new(SmpBootData::new()))
    }

    /// Returns a mutable reference to the inner [`SmpBootData`].
    ///
    /// # Safety
    ///
    /// Caller must ensure exclusive access on the bootstrap processor prior to bringing up
    /// secondary CPUs, and that no other reference (`&` or `&mut`) to the inner data exists for
    /// the lifetime of the returned reference.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self) -> &mut SmpBootData {
        // SAFETY: Caller guarantees exclusive access with no aliasing during boot initialization.
        unsafe { &mut *self.0.get() }
    }
}

// SAFETY: `BootCell` is only accessed on the bootstrap processor during single-CPU boot
// initialization (`traverse_topology` at `LK_INIT_LEVEL_TOPOLOGY` and `platform_init_smp` at
// `LK_INIT_LEVEL_PLATFORM` prior to bringing up secondary CPUs).
unsafe impl Sync for BootCell {}

static SMP_BOOT_DATA: BootCell = BootCell::new();

/// Initializes the global `CpuDistanceMap` with distances computed by `f`.
fn cpu_distance_map_initialize<F>(cpu_count: usize, mut f: F)
where
    F: FnMut(cpu_num_t, cpu_num_t) -> u32,
{
    extern "C" fn trampoline<F>(from_id: cpu_num_t, to_id: cpu_num_t, ctx: *mut c_void) -> u32
    where
        F: FnMut(cpu_num_t, cpu_num_t) -> u32,
    {
        // SAFETY: `ctx` is a valid pointer to `f` for the duration of `cpp_cpu_distance_map_init`.
        let cb = unsafe { &mut *ctx.cast::<F>() };
        cb(from_id, to_id)
    }

    let ctx = core::ptr::from_mut(&mut f).cast::<c_void>();
    // SAFETY: `trampoline::<F>` and `ctx` are valid for the synchronous callbacks made during
    // `cpp_cpu_distance_map_init`.
    unsafe {
        cpp_cpu_distance_map_init(cpu_count, trampoline::<F>, ctx);
    }
}

/// Discovers active CPUs from the system topology graph and initializes the CPU distance map.
fn traverse_topology(_level: init::LkInitLevel) {
    // Filter out hyperthreads if we've been told not to init them
    let use_ht = BootOptions::get().smp_ht_enabled;

    // We're implicitly running on the BSP
    let bsp_apic_id = apic_local_id() as u32;
    debug_assert_eq!(bsp_apic_id, apic_bsp_id() as u32);

    // Maps from contiguous id to logical id in topology.
    let mut logical_ids = fbl::Vector::<cpu_num_t>::new();
    // SAFETY: `traverse_topology` runs once on the BSP at `LK_INIT_LEVEL_TOPOLOGY`, with no
    // concurrent or re-entrant access to `SMP_BOOT_DATA`.
    let smp_data = unsafe { SMP_BOOT_DATA.get_mut() };

    // Iterate over all the cores, copy apic ids of active cores into list.
    dprintf!(INFO, "cpu list:\n");
    let mut cpu_index: usize = 0;
    smp_data.bsp_apic_id_index = 0;
    for processor_node in topology::get_system_topology().processors() {
        let zbi::TopologyEntity::Processor(processor) = processor_node.entity() else {
            panic!("Invalid processor node.");
        };
        let zbi::TopologyArchitectureInfo::X64(x64) = &processor.architecture_info else {
            panic!("Invalid processor node.");
        };
        for (i, &apic_id) in x64.apic_ids.iter().enumerate().take(x64.apic_id_count as usize) {
            let keep = (i < 1) || use_ht;
            let index = cpu_index;
            cpu_index += 1;

            dprintf!(
                INFO,
                "\t{:3}: apic id {:#4x} {}{}{}\n",
                index,
                apic_id,
                if i > 0 { "SMT " } else { "" },
                if apic_id == bsp_apic_id { "BSP " } else { "" },
                if keep { "" } else { "(not using)" }
            );

            if keep {
                if apic_id == bsp_apic_id {
                    smp_data.bsp_apic_id_index = smp_data.apic_ids.len();
                }

                if smp_data.apic_ids.push_back(apic_id).is_err() {
                    dprintf!(CRITICAL, "Failed to allocate apic_ids table, disabling SMP!\n");
                    smp_data.apic_ids = fbl::Vector::new();
                    cpu_distance_map_initialize(1, |_, _| 0);
                    return;
                }
                if logical_ids.push_back(index as cpu_num_t).is_err() {
                    dprintf!(CRITICAL, "Failed to allocate logical_ids table, disabling SMP!\n");
                    smp_data.apic_ids = fbl::Vector::new();
                    cpu_distance_map_initialize(1, |_, _| 0);
                    return;
                }
            }
        }
    }

    // Find the CPU count limit
    let mut max_cpus = BootOptions::get().smp_max_cpus as usize;
    if max_cpus > SMP_MAX_CPUS || max_cpus == 0 {
        kprint!("invalid kernel.smp.maxcpus value, defaulting to {}\n", SMP_MAX_CPUS);
        max_cpus = SMP_MAX_CPUS;
    }

    dprintf!(
        INFO,
        "Found {} cpu{}\n",
        smp_data.apic_ids.len(),
        if smp_data.apic_ids.len() > 1 { 's' } else { ' ' }
    );
    if smp_data.apic_ids.len() > max_cpus {
        dprintf!(INFO, "Clamping number of CPUs to {}\n", max_cpus);
        smp_data.apic_ids.truncate(max_cpus);
        logical_ids.truncate(max_cpus);
    }

    if smp_data.apic_ids.len() == max_cpus || !use_ht {
        // If we are at the max number of CPUs, or have filtered out
        // hyperthreads, safety check the bootstrap processor is in the set.
        assert!(smp_data.apic_ids.contains(&bsp_apic_id));
    }

    // Construct a distance map from the system topology.
    // The passed lambda is call for every pair of logical processors in the system.
    let cpu_count = logical_ids.len();

    // Record the lowest level at which cpus are shared in the hierarchy, used later to
    // set the global distance threshold.
    let mut lowest_sharing_level: u32 = 4; // Start at the highest level we might compute.
    cpu_distance_map_initialize(cpu_count, |from_id, to_id| {
        let logical_from_id = logical_ids[from_id as usize];
        let logical_to_id = logical_ids[to_id as usize];
        let topology: &Graph = topology::get_system_topology();

        let Ok(from_node) = topology.processor_by_logical_id(logical_from_id) else {
            kprint!("Failed to get processor node for logical CPU {}\n", logical_from_id);
            return u32::MAX;
        };

        let Ok(to_node) = topology.processor_by_logical_id(logical_to_id) else {
            kprint!("Failed to get processor node for logical CPU {}\n", logical_to_id);
            return u32::MAX;
        };

        // If the logical cpus are in the same node, they're distance 1
        // TODO: consider SMT as a closer level than cache?
        if core::ptr::eq(from_node, to_node) {
            return 1;
        }

        // Given a level of topology, return true if the two cpus have a shared parent node.
        let is_shared_at_level = |matches_type: fn(&zbi::TopologyEntity) -> bool| -> bool {
            let from_level_node = core::iter::successors(from_node.parent(), |node| node.parent())
                .find(|node| matches_type(node.entity()))
                .map(core::ptr::from_ref);
            let to_level_node = core::iter::successors(to_node.parent(), |node| node.parent())
                .find(|node| matches_type(node.entity()))
                .map(core::ptr::from_ref);

            from_level_node.is_some() && from_level_node == to_level_node
        };

        // If we've detected the same cache node, then we are level 1
        if is_shared_at_level(|e| matches!(e, zbi::TopologyEntity::Cache(_))) {
            lowest_sharing_level = core::cmp::min(lowest_sharing_level, 1);
            return 1;
        }

        // If we're on the same die, we're level 2
        if is_shared_at_level(|e| matches!(e, zbi::TopologyEntity::Die(_))) {
            lowest_sharing_level = core::cmp::min(lowest_sharing_level, 2);
            return 2;
        }

        // If we're on the same socket, we're level 3
        if is_shared_at_level(|e| matches!(e, zbi::TopologyEntity::Socket(_))) {
            lowest_sharing_level = core::cmp::min(lowest_sharing_level, 3);
            return 3;
        }

        // Above socket level is all distance 4
        lowest_sharing_level = core::cmp::min(lowest_sharing_level, 4);
        4
    });

    // Set the point at which we should consider scheduling to be distant. Set it
    // one past the point a which we started seeing some sharing at the cache, die,
    // or socket level.
    // Limitations: does not handle asymmetric topologies, such as hybrid cpus
    // with dissimilar cpu clusters.
    let distance_threshold: u32 = lowest_sharing_level + 1;
    // SAFETY: `CpuDistanceMap` was initialized above.
    unsafe {
        cpp_cpu_distance_map_set_distance_threshold(distance_threshold);
        cpp_cpu_distance_map_dump();
    }
}

init::lk_init_hook!(pc_traverse_topology, traverse_topology, init::LK_INIT_LEVEL_TOPOLOGY);

/// Must be called after `traverse_topology` has processed the SMP data.
fn platform_init_smp() {
    // SAFETY: `platform_init_smp` runs once on the BSP prior to bringing up secondary CPUs, with
    // no concurrent or re-entrant access to `SMP_BOOT_DATA`.
    let smp_data = unsafe { SMP_BOOT_DATA.get_mut() };
    if smp_data.apic_ids.is_empty() {
        return;
    }

    // SAFETY: `smp_data.apic_ids` contains the valid list of APIC IDs discovered in
    // `traverse_topology`.
    unsafe {
        x86_init_smp(smp_data.apic_ids.as_mut_ptr(), smp_data.apic_ids.len() as u32);
    }

    // trim the boot cpu out of the apic id list before passing to the AP booting routine
    smp_data.apic_ids.erase(smp_data.bsp_apic_id_index);

    // SAFETY: `smp_data.apic_ids` contains the remaining APIC IDs to bring up after
    // `x86_init_smp`.
    unsafe {
        let _ = x86_bringup_aps(smp_data.apic_ids.as_ptr(), smp_data.apic_ids.len() as u32);
    }
}

/// Prepare for CPU unplug. The platform may want to shift around external interrupts at this time.
#[unsafe(no_mangle)]
pub extern "C" fn platform_mp_prep_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // TODO: Make sure the IOAPIC and PCI have nothing for this CPU
    // SAFETY: `arch_mp_prep_cpu_unplug` is safe to call with `cpu_id`.
    unsafe { arch_mp_prep_cpu_unplug(cpu_id) }
}

/// Shutdown the specified CPU. Called after it is no longer being scheduled on.
///
/// # Safety
///
/// `cpu_id` must not be the calling CPU and must already be offline/halted and no longer scheduled
/// on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn platform_mp_cpu_unplug(cpu_id: cpu_num_t) -> Result<(), Status> {
    // SAFETY: Caller guarantees `cpu_id` is not the calling CPU and is already offline/halted and
    // no longer being scheduled on.
    unsafe { arch_mp_cpu_unplug(cpu_id) }
}

/// Global C-string pointer wrapper for SMBIOS strings shared with C++ (`extern const char*`).
///
/// `#[repr(transparent)]` over [`zr::AtomicConstPtr<c_char>`] has the exact ABI layout of a C
/// `const char*` global variable while satisfying Rust `Sync` requirements without `static mut`.
#[repr(transparent)]
struct GlobalCStr(zr::AtomicConstPtr<c_char>);

impl GlobalCStr {
    const fn new(s: &'static CStr) -> Self {
        Self(zr::AtomicConstPtr::new(s.as_ptr()))
    }

    fn store(&self, s: &'static CStr) {
        // Relaxed ordering is sufficient because this is written once during `platform_init`
        // before userspace starts and read afterwards.
        self.0.store(s.as_ptr(), Ordering::Relaxed);
    }

    fn as_c_str(&self) -> &'static CStr {
        // Relaxed ordering is sufficient because this is read after being initialized in
        // `platform_init`.
        // SAFETY: `GlobalCStr` is only ever initialized or updated with `'static` `CStr`
        // references (`GlobalCStr::new` and `GlobalCStr::store`).
        unsafe { CStr::from_ptr(self.0.load(Ordering::Relaxed)) }
    }
}

/// Manufacturer and product name pointers specified in the SMBIOS table. Simply set to `"unknown"`
/// in the case where these aren't matchable or the table isn't walk-able, `"<null>"` if the table
/// specifies there isn't a value provided, or `"<missing string>"` if the table specifies a string
/// index that doesn't match the string table.
#[unsafe(export_name = "manufacturer")]
static MANUFACTURER: GlobalCStr = GlobalCStr::new(c"unknown");

/// System product name pointer specified in the SMBIOS table (see [`MANUFACTURER`]).
#[unsafe(export_name = "product")]
static PRODUCT: GlobalCStr = GlobalCStr::new(c"unknown");

/// Manufacturer information, fetched from SMBIOS during system startup.
#[inline]
pub fn manufacturer() -> &'static CStr {
    MANUFACTURER.as_c_str()
}

/// Updates `manufacturer` and `product` if `hdr` describes an SMBIOS v2.0+ System Information
/// structure.
fn update_smbios_system_info(
    version: smbios::SpecVersion,
    hdr: &smbios::Header,
    raw_bytes: &'static [u8],
    st: &smbios::StringTable<'static>,
    manufacturer_out: &GlobalCStr,
    product_out: &GlobalCStr,
) {
    if hdr.r#type == smbios::StructType::SystemInfo
        && version.includes_version(2, 0, 0)
        && let Ok((entry, _)) = smbios::SystemInformationStruct2_0::ref_from_prefix(raw_bytes)
    {
        manufacturer_out.store(st.get_c_str_or_default(usize::from(entry.manufacturer_str_idx)));
        product_out.store(st.get_c_str_or_default(usize::from(entry.product_name_str_idx)));
    }
}

/// Later platform initialization, after the kernel heap is set up.
fn platform_init() {
    platform_init_crashlog();

    // Initialize all PvEoi instances and SMBIOS information prior to starting secondary CPUs.
    PvEoi::init_all();

    super::smbios::pc_init_smbios();

    let _ = super::smbios::walk_structs(|version, h, raw_bytes, st| {
        update_smbios_system_info(version, h, raw_bytes, st, &MANUFACTURER, &PRODUCT);
        Ok(())
    });
    kprint!(
        "smbios: manufacturer=\"{:s}\" product=\"{:s}\"\n",
        MANUFACTURER.as_c_str(),
        PRODUCT.as_c_str()
    );

    platform_init_smp();
}

/// FFI entry point for [`platform_init`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_init() {
    platform_init();
}

/// Queries the power state of `cpu_id`.
fn platform_get_cpu_state(_cpu_id: cpu_num_t) -> Result<PowerCpuState, Status> {
    Err(Status::NOT_SUPPORTED)
}

/// FFI entry point for [`platform_get_cpu_state`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_get_cpu_state(
    cpu_id: cpu_num_t,
    out_state: &mut PowerCpuState,
) -> Result<(), Status> {
    *out_state = platform_get_cpu_state(cpu_id)?;
    Ok(())
}

/// Unit tests for PC platform initialization helpers.
#[cfg(ktest)]
#[unittest::suite(name = "pc_platform_tests")]
#[allow(unused_imports)]
mod tests {
    /// Verifies `GlobalCStr` initialization, store, and retrieval.
    #[test]
    fn test_global_c_str() {
        let cell = GlobalCStr::new(c"unknown");
        assert_eq!(cell.as_c_str(), c"unknown");

        cell.store(c"Fuchsia");
        assert_eq!(cell.as_c_str(), c"Fuchsia");
    }

    /// Verifies `platform_get_cpu_state` returns `Status::NOT_SUPPORTED`.
    #[test]
    fn test_platform_get_cpu_state_not_supported() {
        assert_eq!(platform_get_cpu_state(0), Err(Status::NOT_SUPPORTED));
    }

    /// Verifies SMBIOS System Information v2.0+ manufacturer/product extraction and validation.
    #[test]
    fn test_update_smbios_system_info() {
        static RAW_SYS_INFO: [u8; 8] = [
            1, // type = SystemInfo (1)
            8, // length = 8 (SystemInformationStruct2_0)
            0, 0, // handle = 0
            1, // manufacturer_str_idx = 1
            2, // product_name_str_idx = 2
            0, // version_str_idx = 0
            0, // serial_number_str_idx = 0
        ];
        static RAW_STRUCT_AND_STRINGS: [u8; 24] =
            *b"\x01\x08\x00\x00\x01\x02\x00\x00Google\0PC\0\0\0\0\0\0\0";

        let (hdr, _) = smbios::Header::ref_from_prefix(&RAW_SYS_INFO).unwrap();
        let st =
            smbios::StringTable::init(hdr, RAW_STRUCT_AND_STRINGS.len(), &RAW_STRUCT_AND_STRINGS)
                .unwrap();

        let mfr = GlobalCStr::new(c"unknown");
        let prod = GlobalCStr::new(c"unknown");

        // Version < 2.0 should not update.
        update_smbios_system_info(
            smbios::SpecVersion::new(1, 9, 0),
            hdr,
            &RAW_SYS_INFO,
            &st,
            &mfr,
            &prod,
        );
        assert_eq!(mfr.as_c_str(), c"unknown");
        assert_eq!(prod.as_c_str(), c"unknown");

        // Truncated raw_bytes (< 8 bytes) should not update.
        update_smbios_system_info(
            smbios::SpecVersion::new(2, 1, 0),
            hdr,
            &RAW_SYS_INFO[..4],
            &st,
            &mfr,
            &prod,
        );
        assert_eq!(mfr.as_c_str(), c"unknown");
        assert_eq!(prod.as_c_str(), c"unknown");

        // Valid v2.1 SystemInfo should update manufacturer and product.
        update_smbios_system_info(
            smbios::SpecVersion::new(2, 1, 0),
            hdr,
            &RAW_SYS_INFO,
            &st,
            &mfr,
            &prod,
        );
        assert_eq!(mfr.as_c_str(), c"Google");
        assert_eq!(prod.as_c_str(), c"PC");
    }
}
