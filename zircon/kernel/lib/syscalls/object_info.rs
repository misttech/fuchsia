// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::{UserCopyCaptureFaultsError, max_num_cpus};
use crate::kernel::mp::is_cpu_online;
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler::get_projected_cpu_stats;
use crate::object::{
    BusTransactionInitiatorDispatcher, ClockDispatcher, Dispatcher, HandleValue,
    InterruptDispatcher, IoBufferDispatcher, JobDispatcher, MsiDispatcher, ProcessDispatcher,
    ResourceDispatcher, SocketDispatcher, StreamDispatcher, ThreadDispatcher, TimerDispatcher,
    VcpuDispatcher, VmAddressRegionDispatcher, VmObjectDispatcher, VmarMapsInfoWriter,
    VmoInfoWriter, get_vmar_maps, validate_ranged_resource,
};
use crate::user_copy::UserOutPtr;
use crate::vm::memory_stats::{
    MapsInfoVersion, VmoInfoVersion, get_compression_stats, get_memory_stats, get_stall_stats,
    kernel_stats_info_to_version, maps_info_to_version, vmo_info_to_version,
};
use debug::ltracef;
use fbl::Array;
use syscalls_macro::syscall;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::*;

const LOCAL_TRACE: u32 = 0;

// Specialize the VmoInfoWriter to work for any T that is a subset of zx_info_vmo_t. This is
// currently true for v1 and v2 (v2 being the current version). Being a subset the full
// zx_info_vmo_t can just be casted and copied.
struct SubsetVmoInfoWriter<T> {
    out: UserOutPtr<T>,
    base_offset: usize,
}

impl<T: VmoInfoVersion + IntoBytes + Immutable> SubsetVmoInfoWriter<T> {
    const _SIZE_ASSERT: () =
        assert!(core::mem::size_of::<T>() <= core::mem::size_of::<zx_info_vmo_t>());

    fn new(out: UserOutPtr<T>) -> Self {
        let () = Self::_SIZE_ASSERT;
        Self { out, base_offset: 0 }
    }
}

impl<T: VmoInfoVersion + IntoBytes + Immutable> VmoInfoWriter for SubsetVmoInfoWriter<T> {
    fn write(&mut self, vmo: &zx_info_vmo_t, offset: usize) -> Result<(), Status> {
        let versioned_vmo: T = vmo_info_to_version(vmo);
        self.out.element_offset(offset + self.base_offset).copy_to_user(&versioned_vmo)
    }

    fn write_capture_faults(
        &mut self,
        vmo: &zx_info_vmo_t,
        offset: usize,
    ) -> Result<(), UserCopyCaptureFaultsError> {
        let versioned_vmo: T = vmo_info_to_version(vmo);
        self.out
            .element_offset(offset + self.base_offset)
            .copy_to_user_capture_faults(&versioned_vmo)
    }

    fn add_offset(&mut self, offset: usize) {
        self.base_offset += offset;
    }
}

struct SubsetVmarMapsInfoWriter<T> {
    out: UserOutPtr<T>,
    base_offset: usize,
}

impl<T: MapsInfoVersion + IntoBytes + Immutable> SubsetVmarMapsInfoWriter<T> {
    const _SIZE_ASSERT: () =
        assert!(core::mem::size_of::<T>() <= core::mem::size_of::<zx_info_maps_t>());

    fn new(out: UserOutPtr<T>) -> Self {
        let () = Self::_SIZE_ASSERT;
        Self { out, base_offset: 0 }
    }
}

impl<T: MapsInfoVersion + IntoBytes + Immutable> VmarMapsInfoWriter
    for SubsetVmarMapsInfoWriter<T>
{
    fn write(&mut self, maps: &zx_info_maps_t, offset: usize) -> Result<(), Status> {
        let versioned_maps: T = maps_info_to_version(maps);
        self.out.element_offset(offset + self.base_offset).copy_to_user(&versioned_maps)
    }

    fn write_capture_faults(
        &mut self,
        maps: &zx_info_maps_t,
        offset: usize,
    ) -> Result<(), UserCopyCaptureFaultsError> {
        let versioned_maps: T = maps_info_to_version(maps);
        self.out
            .element_offset(offset + self.base_offset)
            .copy_to_user_capture_faults(&versioned_maps)
    }

    fn add_offset(&mut self, offset: usize) {
        self.base_offset += offset;
    }
}

// TODO: figure out a better handle to hang this off to and push this copy code into
// that dispatcher.
fn get_cpu_stats(cpu_num: u32) -> zx_info_cpu_stats_t {
    let cpu_stats = get_projected_cpu_stats(cpu_num);
    zx_info_cpu_stats_t {
        cpu_number: cpu_num,
        flags: if is_cpu_online(cpu_num) { ZX_INFO_CPU_STATS_FLAG_ONLINE } else { 0 },
        idle_time: cpu_stats.idle_time,
        normalized_busy_time: cpu_stats.normalized_busy_time,
        reschedules: cpu_stats.reschedules,
        context_switches: cpu_stats.context_switches,
        irq_preempts: 0, // deprecated, not tracking different types of preemptions.
        preempts: cpu_stats.preempts,
        yields: cpu_stats.yields,
        ints: cpu_stats.interrupts,
        timer_ints: cpu_stats.timer_ints,
        timers: cpu_stats.timers,
        page_faults: cpu_stats.page_faults,
        exceptions: 0, // deprecated, use "kcounter" command for now.
        syscalls: cpu_stats.syscalls,
        reschedule_ipis: cpu_stats.reschedule_ipis,
        generic_ipis: cpu_stats.generic_ipis,
        active_energy_consumption_nj: cpu_stats.active_energy_consumption_nj,
        idle_energy_consumption_nj: cpu_stats.idle_energy_consumption_nj,
    }
}

fn get_guest_cpu_stats(cpu_num: u32) -> zx_info_guest_stats_t {
    let cpu = PerCpu::get(cpu_num);
    #[cfg_attr(target_arch = "riscv64", allow(unused_mut))]
    let mut stats = zx_info_guest_stats_t {
        cpu_number: cpu_num,
        flags: if is_cpu_online(cpu_num) { ZX_INFO_CPU_STATS_FLAG_ONLINE } else { 0 },
        vm_entries: cpu.gstats.vm_entries,
        vm_exits: cpu.gstats.vm_exits,
        ..Default::default()
    };
    #[cfg(target_arch = "aarch64")]
    {
        stats.wfi_wfe_instructions = cpu.gstats.wfi_wfe_instructions;
        stats.system_instructions = cpu.gstats.system_instructions;
        stats.instruction_aborts = cpu.gstats.instruction_aborts;
        stats.data_aborts = cpu.gstats.data_aborts;
        stats.smc_instructions = cpu.gstats.smc_instructions;
        stats.interrupts = cpu.gstats.interrupts;
    }
    #[cfg(target_arch = "x86_64")]
    {
        stats.vmcall_instructions = cpu.gstats.vmcall_instructions;
        stats.pause_instructions = cpu.gstats.pause_instructions;
        stats.xsetbv_instructions = cpu.gstats.xsetbv_instructions;
        stats.ept_violations = cpu.gstats.ept_violations;
        stats.wrmsr_instructions = cpu.gstats.wrmsr_instructions;
        stats.rdmsr_instructions = cpu.gstats.rdmsr_instructions;
        stats.io_instructions = cpu.gstats.io_instructions;
        stats.control_register_accesses = cpu.gstats.control_register_accesses;
        stats.hlt_instructions = cpu.gstats.hlt_instructions;
        stats.cpuid_instructions = cpu.gstats.cpuid_instructions;
        stats.interrupt_windows = cpu.gstats.interrupt_windows;
        stats.interrupts = cpu.gstats.interrupts;
    }
    stats
}

// Current second version is an extension of v1; simply copy over the
// earlier header and context.arch fields.
fn convert_thread_exception_report_v1(info: &zx_exception_report_t) -> zx_exception_report_v1_t {
    let (v1, _) = zx_exception_report_v1_t::read_from_prefix(info.as_bytes())
        .expect("zx_exception_report_t is larger than zx_exception_report_v1_t");
    v1
}

// Copies to usermode the actual (number of records written) and the avail (number of records
// available).
fn actual_avail_result(
    actual: usize,
    avail: usize,
    user_actual: UserOutPtr<usize>,
    user_avail: UserOutPtr<usize>,
) -> Result<(), Status> {
    if !user_actual.is_null() {
        user_actual.write(actual)?;
    }
    if !user_avail.is_null() {
        user_avail.write(avail)?;
    }
    Ok(())
}

// Copies a single record, |src_record|, into the user buffer |dst_buffer| of size
// |dst_buffer_size|. If the copy succeeds, the value 1 is copied into |user_avail|.
#[inline(always)]
fn single_record_result<T: IntoBytes + Immutable>(
    dst_buffer: UserOutPtr<u8>,
    dst_buffer_size: usize,
    user_actual: UserOutPtr<usize>,
    user_avail: UserOutPtr<usize>,
    src_record: &T,
) -> Result<(), Status> {
    single_record_bytes_result(
        dst_buffer,
        dst_buffer_size,
        user_actual,
        user_avail,
        src_record.as_bytes(),
    )
}

fn single_record_bytes_result(
    dst_buffer: UserOutPtr<u8>,
    dst_buffer_size: usize,
    user_actual: UserOutPtr<usize>,
    user_avail: UserOutPtr<usize>,
    src_bytes: &[u8],
) -> Result<(), Status> {
    let actual = if dst_buffer_size >= src_bytes.len() {
        dst_buffer.copy_slice_to_user(src_bytes).map_err(|_| Status::INVALID_ARGS)?;
        1
    } else {
        0
    };
    actual_avail_result(actual, 1, user_actual, user_avail)?;
    if actual == 0 {
        return Err(Status::BUFFER_TOO_SMALL);
    }
    Ok(())
}

// Copies to usermode an (fbl) array of results to |dst_buffer| up to |dst_buffer_size|. It uses
// actual_avail_result() to copy to usermode the available and actual copied records.
fn multi_record_result<T: IntoBytes + Immutable>(
    dst_buffer: UserOutPtr<u8>,
    dst_buffer_size: usize,
    user_actual: UserOutPtr<usize>,
    user_avail: UserOutPtr<usize>,
    src_array: &Array<T>,
) -> Result<(), Status> {
    let avail = src_array.len();
    let num_space_for = dst_buffer_size / core::mem::size_of::<T>();
    let actual = avail.min(num_space_for);
    // Don't try to copy if there are no bytes to copy, as the "is
    // user space" check may not handle (_buffer == NULL and len == 0).
    if actual > 0 {
        dst_buffer
            .reinterpret::<T>()
            .copy_slice_to_user(&src_array[..actual])
            .map_err(|_| Status::INVALID_ARGS)?;
    }
    actual_avail_result(actual, avail, user_actual, user_avail)
}

fn get_handle_stats(process: &ProcessDispatcher) -> Result<zx_info_process_handle_stats_t, Status> {
    let mut info = zx_info_process_handle_stats_t::default();
    const {
        assert!(
            zx_info_process_handle_stats_t { handle_count: [0; _] }.handle_count.len()
                >= ZX_OBJ_TYPE_UPPER_BOUND,
            "Need room for each handle type."
        );
    };

    let _ = process.handle_table().for_each_handle(|_handle, _rights, dispatcher| {
        info.handle_count[dispatcher.get_type() as usize] += 1;
        Ok(())
    });
    Ok(info)
}

// actual is an optional return parameter for the number of records returned
// avail is an optional return parameter for the number of records available
//
// Topics which return a fixed number of records will return ZX_ERR_BUFFER_TOO_SMALL
// if there is not enough buffer space provided.
// This allows for zx_object_get_info(handle, topic, &info, sizeof(info), NULL, NULL)
#[syscall]
pub fn sys_object_get_info(
    handle: HandleValue,
    topic: u32,
    buffer: UserOutPtr<u8>,
    buffer_size: usize,
    actual: UserOutPtr<usize>,
    avail: UserOutPtr<usize>,
) -> Result<(), Status> {
    ltracef!("handle {:?} topic {}\n", handle, topic);

    let up = ProcessDispatcher::get_current();

    // Helper macro for single-record topic queries that obtain a typed dispatcher with
    // `ZX_RIGHT_INSPECT` (or rights) and copy the single record into user buffers using
    // `single_record_result`.
    macro_rules! single_record_info {
        // Infallible method call on a dispatcher with ZX_RIGHT_INSPECT.
        ($dispatcher_type:ty, $method:ident) => {{
            let dispatcher = up.handle_table().get_dispatcher_with_rights::<$dispatcher_type>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;
            let record = dispatcher.$method();
            single_record_result(buffer, buffer_size, actual, avail, &record)?;
            Ok(())
        }};
        // Fallible method call on a dispatcher with ZX_RIGHT_INSPECT.
        ($dispatcher_type:ty, $method:ident?) => {{
            let dispatcher = up.handle_table().get_dispatcher_with_rights::<$dispatcher_type>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;
            let record = dispatcher.$method()?;
            single_record_result(buffer, buffer_size, actual, avail, &record)?;
            Ok(())
        }};
        // Custom closure/expression receiving the dispatcher with ZX_RIGHT_INSPECT.
        ($dispatcher_type:ty, |$disp:ident| $body:expr) => {{
            let $disp = up.handle_table().get_dispatcher_with_rights::<$dispatcher_type>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;
            let record = $body;
            single_record_result(buffer, buffer_size, actual, avail, &record)?;
            Ok(())
        }};
        // Closure/expression with handle rights lookup.
        (with_rights: |$disp:ident, $rights:ident| $body:expr) => {{
            let ($disp, $rights) =
                up.handle_table().get_dispatcher_and_rights::<Dispatcher>(&up, handle)?;
            let record = $body;
            single_record_result(buffer, buffer_size, actual, avail, &record)?;
            Ok(())
        }};
        // Version that takes an expression and validates the handle using the base resource
        // of kind ZX_RSRC_KIND_SYSTEM.
        (system_resource: $base:expr, $expr:expr) => {{
            validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, $base, 1)?;
            let record = $expr;
            single_record_result(buffer, buffer_size, actual, avail, &record)?;
            Ok(())
        }};
    }

    match topic {
        ZX_INFO_HANDLE_VALID => {
            // This syscall + topic is excepted from the ZX_POL_BAD_HANDLE policy.
            let _ = up
                .handle_table()
                .get_dispatcher_with_rights_no_policy_check::<Dispatcher>(handle, ZX_RIGHT_NONE)?;
            Ok(())
        }
        ZX_INFO_HANDLE_BASIC => {
            single_record_info!(with_rights: |dispatcher, rights| {
                dispatcher.get_handle_info(rights)
            })
        }
        ZX_INFO_PROCESS => single_record_info!(ProcessDispatcher, get_info),
        ZX_INFO_PROCESS_THREADS => {
            let process = up.handle_table().get_dispatcher_with_rights::<ProcessDispatcher>(
                &up,
                handle,
                ZX_RIGHT_ENUMERATE,
            )?;

            // Getting the list of threads is inherently racy (unless the caller has already stopped
            // all threads.
            let threads = process.get_threads()?;

            multi_record_result(buffer, buffer_size, actual, avail, &threads)
        }
        ZX_INFO_JOB_CHILDREN | ZX_INFO_JOB_PROCESSES => {
            let job = up.handle_table().get_dispatcher_with_rights::<JobDispatcher>(
                &up,
                handle,
                ZX_RIGHT_ENUMERATE,
            )?;
            // Don't recurse; we only want the job's direct children.
            let max =
                if buffer_size == 0 { 0 } else { buffer_size / core::mem::size_of::<zx_koid_t>() };
            let is_jobs = topic == ZX_INFO_JOB_CHILDREN;
            let (count, avail_count) =
                job.enumerate_children(buffer.reinterpret(), max, is_jobs)?;
            actual_avail_result(count, avail_count, actual, avail)?;
            Ok(())
        }
        ZX_INFO_THREAD => single_record_info!(ThreadDispatcher, get_info_for_userspace),
        ZX_INFO_THREAD_EXCEPTION_REPORT_V1 => {
            single_record_info!(ThreadDispatcher, |thread| {
                let report = thread.get_exception_report()?;
                convert_thread_exception_report_v1(&report)
            })
        }
        ZX_INFO_THREAD_EXCEPTION_REPORT => {
            single_record_info!(ThreadDispatcher, get_exception_report?)
        }
        ZX_INFO_THREAD_STATS => single_record_info!(ThreadDispatcher, get_stats_for_userspace?),
        ZX_INFO_TASK_STATS => single_record_info!(ProcessDispatcher, get_stats?),
        ZX_INFO_TASK_STATS_V1 => {
            single_record_info!(ProcessDispatcher, |process| {
                let info = process.get_stats()?;
                zx_info_task_stats_v1_t {
                    mem_mapped_bytes: info.mem_mapped_bytes,
                    mem_private_bytes: info.mem_private_bytes,
                    mem_shared_bytes: info.mem_shared_bytes,
                    mem_scaled_shared_bytes: info.mem_scaled_shared_bytes,
                }
            })
        }
        ZX_INFO_TASK_RUNTIME | ZX_INFO_TASK_RUNTIME_V1 => {
            let dispatcher = up.handle_table().get_dispatcher_with_rights::<Dispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;
            let runtime: zx_info_task_runtime_t =
                if let Some(job) = dispatcher.downcast::<JobDispatcher>() {
                    job.get_runtime_stats()
                } else if let Some(process) = dispatcher.downcast::<ProcessDispatcher>() {
                    process.get_runtime_stats()
                } else if let Some(thread) = dispatcher.downcast::<ThreadDispatcher>() {
                    thread.get_runtime_stats()
                } else {
                    return Err(Status::WRONG_TYPE);
                };
            if topic == ZX_INFO_TASK_RUNTIME_V1 {
                let v1 = zx_info_task_runtime_v1_t {
                    cpu_time: runtime.cpu_time,
                    queue_time: runtime.queue_time,
                };
                single_record_result(buffer, buffer_size, actual, avail, &v1)?;
            } else {
                single_record_result(buffer, buffer_size, actual, avail, &runtime)?;
            }
            Ok(())
        }
        ZX_INFO_PROCESS_MAPS_V1 | ZX_INFO_PROCESS_MAPS_V2 | ZX_INFO_PROCESS_MAPS => {
            let process = up.handle_table().get_dispatcher_with_rights::<ProcessDispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;

            let (count, avail_count) = if topic == ZX_INFO_PROCESS_MAPS_V1 {
                let mut writer =
                    SubsetVmarMapsInfoWriter::<zx_info_maps_v1_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_maps_v1_t>();
                process.get_aspace_maps(&mut writer, max)?
            } else if topic == ZX_INFO_PROCESS_MAPS_V2 {
                let mut writer =
                    SubsetVmarMapsInfoWriter::<zx_info_maps_v2_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_maps_v2_t>();
                process.get_aspace_maps(&mut writer, max)?
            } else {
                let mut writer =
                    SubsetVmarMapsInfoWriter::<zx_info_maps_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_maps_t>();
                process.get_aspace_maps(&mut writer, max)?
            };
            actual_avail_result(count, avail_count, actual, avail)
        }
        ZX_INFO_PROCESS_VMOS_V1
        | ZX_INFO_PROCESS_VMOS_V2
        | ZX_INFO_PROCESS_VMOS_V3
        | ZX_INFO_PROCESS_VMOS => {
            let process = up.handle_table().get_dispatcher_with_rights::<ProcessDispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;

            let (count, avail_count) = if topic == ZX_INFO_PROCESS_VMOS_V1 {
                let mut writer = SubsetVmoInfoWriter::<zx_info_vmo_v1_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_vmo_v1_t>();
                process.get_vmos(&mut writer, max)?
            } else if topic == ZX_INFO_PROCESS_VMOS_V2 {
                let mut writer = SubsetVmoInfoWriter::<zx_info_vmo_v2_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_vmo_v2_t>();
                process.get_vmos(&mut writer, max)?
            } else if topic == ZX_INFO_PROCESS_VMOS_V3 {
                let mut writer = SubsetVmoInfoWriter::<zx_info_vmo_v3_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_vmo_v3_t>();
                process.get_vmos(&mut writer, max)?
            } else {
                let mut writer = SubsetVmoInfoWriter::<zx_info_vmo_t>::new(buffer.reinterpret());
                let max = buffer_size / core::mem::size_of::<zx_info_vmo_t>();
                process.get_vmos(&mut writer, max)?
            };
            actual_avail_result(count, avail_count, actual, avail)
        }
        ZX_INFO_VMO | ZX_INFO_VMO_V1 | ZX_INFO_VMO_V2 | ZX_INFO_VMO_V3 => {
            let (vmo, rights) =
                up.handle_table().get_dispatcher_and_rights::<VmObjectDispatcher>(&up, handle)?;
            let info = vmo.get_vmo_info(rights);
            match topic {
                ZX_INFO_VMO_V1 => {
                    let v1: zx_info_vmo_v1_t = vmo_info_to_version(&info);
                    single_record_result(buffer, buffer_size, actual, avail, &v1)?;
                }
                ZX_INFO_VMO_V2 => {
                    let v2: zx_info_vmo_v2_t = vmo_info_to_version(&info);
                    single_record_result(buffer, buffer_size, actual, avail, &v2)?;
                }
                ZX_INFO_VMO_V3 => {
                    let v3: zx_info_vmo_v3_t = vmo_info_to_version(&info);
                    single_record_result(buffer, buffer_size, actual, avail, &v3)?;
                }
                _ => {
                    single_record_result(buffer, buffer_size, actual, avail, &info)?;
                }
            }
            Ok(())
        }
        ZX_INFO_VMAR => single_record_info!(VmAddressRegionDispatcher, get_vmar_info),
        ZX_INFO_VMAR_MAPS => {
            let vmar = up.handle_table().get_dispatcher_with_rights::<VmAddressRegionDispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;

            let mut writer = SubsetVmarMapsInfoWriter::<zx_info_maps_t>::new(buffer.reinterpret());
            let max_records = buffer_size / core::mem::size_of::<zx_info_maps_t>();
            let (actual_records, avail_records) =
                get_vmar_maps(vmar.vmar(), &mut writer, max_records)?;

            actual_avail_result(actual_records, avail_records, actual, avail)
        }
        ZX_INFO_GUEST_STATS => {
            validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_INFO_BASE, 1)?;

            let num_cpus = max_num_cpus() as usize;
            let num_space_for = buffer_size / core::mem::size_of::<zx_info_guest_stats_t>();
            let num_to_copy = num_cpus.min(num_space_for);
            let guest_buf = buffer.reinterpret::<zx_info_guest_stats_t>();

            for i in 0..num_to_copy {
                let stats = get_guest_cpu_stats(i as u32);
                guest_buf
                    .element_offset(i)
                    .copy_to_user(&stats)
                    .map_err(|_| Status::INVALID_ARGS)?;
            }
            actual_avail_result(num_to_copy, num_cpus, actual, avail)
        }
        ZX_INFO_CPU_STATS => {
            validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_INFO_BASE, 1)?;

            let num_cpus = max_num_cpus() as usize;
            let num_space_for = buffer_size / core::mem::size_of::<zx_info_cpu_stats_t>();
            let num_to_copy = num_cpus.min(num_space_for);
            // build an alias to the output buffer that is in units of the cpu stat structure
            let cpu_buf = buffer.reinterpret::<zx_info_cpu_stats_t>();

            for i in 0..num_to_copy {
                let stats = get_cpu_stats(i as u32);
                cpu_buf.element_offset(i).copy_to_user(&stats).map_err(|_| Status::INVALID_ARGS)?;
            }
            actual_avail_result(num_to_copy, num_cpus, actual, avail)
        }
        ZX_INFO_KMEM_STATS => {
            single_record_info!(system_resource: ZX_RSRC_SYSTEM_INFO_BASE, get_memory_stats())
        }
        ZX_INFO_KMEM_STATS_EXTENDED => {
            single_record_info!(
                system_resource: ZX_RSRC_SYSTEM_INFO_BASE,
                kernel_stats_info_to_version::<zx_info_kmem_stats_extended_t>(&get_memory_stats())
            )
        }
        ZX_INFO_KMEM_STATS_V1 => {
            single_record_info!(
                system_resource: ZX_RSRC_SYSTEM_INFO_BASE,
                kernel_stats_info_to_version::<zx_info_kmem_stats_v1_t>(&get_memory_stats())
            )
        }
        ZX_INFO_KMEM_STATS_COMPRESSION => {
            single_record_info!(system_resource: ZX_RSRC_SYSTEM_INFO_BASE, get_compression_stats())
        }
        ZX_INFO_MEMORY_STALL => {
            single_record_info!(system_resource: ZX_RSRC_SYSTEM_STALL_BASE, get_stall_stats())
        }
        ZX_INFO_RESOURCE => single_record_info!(ResourceDispatcher, get_info),
        ZX_INFO_HANDLE_COUNT => {
            single_record_info!(Dispatcher, |dispatcher| {
                zx_info_handle_count_t { handle_count: dispatcher.current_handle_count() }
            })
        }
        ZX_INFO_PROCESS_HANDLE_STATS => {
            single_record_info!(ProcessDispatcher, |process| get_handle_stats(&process)?)
        }
        ZX_INFO_HANDLE_TABLE => {
            let process = up.handle_table().get_dispatcher_with_rights::<ProcessDispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT | ZX_RIGHT_MANAGE_PROCESS | ZX_RIGHT_MANAGE_THREAD,
            )?;

            if buffer.is_null() && avail.is_null() && !actual.is_null() {
                // Optimization for callers which call twice, the first time just to know the size.
                return actual.write(up.handle_table().handle_count() as usize);
            }
            let handle_info = process.handle_table().get_handle_info()?;

            multi_record_result(buffer, buffer_size, actual, avail, &handle_info)
        }
        ZX_INFO_SOCKET => single_record_info!(SocketDispatcher, get_info),
        ZX_INFO_JOB => single_record_info!(JobDispatcher, get_info),
        ZX_INFO_TIMER => single_record_info!(TimerDispatcher, get_info),
        ZX_INFO_STREAM => single_record_info!(StreamDispatcher, get_info),
        ZX_INFO_MSI => single_record_info!(MsiDispatcher, get_info),
        ZX_INFO_BTI => single_record_info!(BusTransactionInitiatorDispatcher, get_info),
        ZX_INFO_VCPU => single_record_info!(VcpuDispatcher, get_info),
        ZX_INFO_IOB => single_record_info!(IoBufferDispatcher, get_info),
        ZX_INFO_IOB_REGIONS => {
            let iob = up.handle_table().get_dispatcher_with_rights::<IoBufferDispatcher>(
                &up,
                handle,
                ZX_RIGHT_INSPECT,
            )?;

            let num_regions = iob.region_count();
            let num_space_for = buffer_size / core::mem::size_of::<zx_iob_region_info_t>();
            let num_to_copy = num_regions.min(num_space_for);

            for i in 0..num_to_copy {
                let region = iob.get_region_info(i);
                buffer
                    .reinterpret::<zx_iob_region_info_t>()
                    .element_offset(i)
                    .copy_to_user(&region)?;
            }
            actual_avail_result(num_to_copy, num_regions, actual, avail)
        }
        ZX_INFO_CLOCK_MAPPED_SIZE => single_record_info!(ClockDispatcher, get_mapped_size?),
        ZX_INFO_INTERRUPT => single_record_info!(InterruptDispatcher, get_info),
        _ => Err(Status::NOT_SUPPORTED),
    }
}
