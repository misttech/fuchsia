// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::PAGE_SIZE;
use crate::task::dynamic_thread_spawner::SpawnRequestBuilder;
use anyhow::Context;
use fidl_fuchsia_cpu_profiler as profiler;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_runtime;
use futures::StreamExt;
use futures::channel::mpsc as future_mpsc;
use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc as sync_mpsc};
use zerocopy::{Immutable, IntoBytes};
#[cfg(target_arch = "aarch64")]
use zx::sys::zx_restricted_state_t;

use fxt::TraceRecord;
use fxt::profiler::ProfilerRecord;
use fxt::session::SessionParser;
use seq_lock::{SeqLock, SeqLockable, WriteSize};
use starnix_logging::{log_error, log_info, log_warn, track_stub};
use starnix_sync::{LockDepMutex, LockDepRwLock, PerfEventLevel, PerfFormatIdLookupTableLock};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::arch32::{
    PERF_EVENT_IOC_DISABLE, PERF_EVENT_IOC_ENABLE, PERF_EVENT_IOC_ID,
    PERF_EVENT_IOC_MODIFY_ATTRIBUTES, PERF_EVENT_IOC_PAUSE_OUTPUT, PERF_EVENT_IOC_PERIOD,
    PERF_EVENT_IOC_QUERY_BPF, PERF_EVENT_IOC_REFRESH, PERF_EVENT_IOC_RESET, PERF_EVENT_IOC_SET_BPF,
    PERF_EVENT_IOC_SET_FILTER, PERF_EVENT_IOC_SET_OUTPUT, PERF_RECORD_MISC_USER,
    perf_event_sample_format_PERF_SAMPLE_CALLCHAIN, perf_event_sample_format_PERF_SAMPLE_ID,
    perf_event_sample_format_PERF_SAMPLE_IDENTIFIER, perf_event_sample_format_PERF_SAMPLE_IP,
    perf_event_sample_format_PERF_SAMPLE_PERIOD, perf_event_sample_format_PERF_SAMPLE_READ,
    perf_event_sample_format_PERF_SAMPLE_REGS_USER,
    perf_event_sample_format_PERF_SAMPLE_STACK_USER, perf_event_sample_format_PERF_SAMPLE_TID,
    perf_event_sample_format_PERF_SAMPLE_TIME, perf_event_type_PERF_RECORD_LOST,
    perf_event_type_PERF_RECORD_SAMPLE,
};
use starnix_uapi::auth::{CAP_PERFMON, CAP_SYS_ADMIN};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::restricted_aspace::RESTRICTED_ASPACE_HIGHEST_ADDRESS;
use starnix_uapi::uapi::{
    perf_callchain_context_PERF_CONTEXT_KERNEL, perf_callchain_context_PERF_CONTEXT_USER,
    perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64,
    perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE,
};
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{
    errno, error, from_status_like_fdio, perf_event_attr, perf_event_header,
    perf_event_mmap_page__bindgen_ty_1, perf_event_read_format_PERF_FORMAT_GROUP,
    perf_event_read_format_PERF_FORMAT_ID, perf_event_read_format_PERF_FORMAT_LOST,
    perf_event_read_format_PERF_FORMAT_TOTAL_TIME_ENABLED,
    perf_event_read_format_PERF_FORMAT_TOTAL_TIME_RUNNING, tid_t, uapi,
};

use crate::security::{self, PerfEventOpenType, PerfEventTarget, PerfEventType};
use crate::task::Kernel;
use crate::task::tracing::{LinuxIdentity, PidKoidSession};

static READ_FORMAT_ID_GENERATOR: AtomicU64 = AtomicU64::new(0);
// Size of the VMO backing each event's metadata page and ring buffer; mmap
// lengths up to this size are accepted. perf readers map one metadata page
// plus a power-of-two data area, and commonly ask for megabytes (e.g. 256
// data pages or more), so leave generous headroom. With circular writes the
// data area no longer needs to hold a whole session's records.
//
// We currently preallocate a fixed size for the VMO because we do not dynamically
// resize it when mapped.
// TODO(https://fxbug.dev/540986386): Support dynamic resizing or pass the requested size.
const ESTIMATED_MMAP_BUFFER_SIZE: u64 = 16 * 1024 * 1024;
// Size of a PERF_RECORD_LOST record in bytes:
// perf_event_header (8) + sample_id (8) + lost_events (8) = 24.
const LOST_RECORD_SIZE: u64 = 24;
// Register indices in the profiler's register capture block.
const AARCH64_REG_PC: usize = 32;
const AARCH64_REG_SP: usize = 31;
const AARCH64_REG_LR: usize = 30;
// AArch32 state arrives in the same block, R0-R14 in x0-x14 (the Linux
// compat layout); the AArch32 PC, LR and SP are R15, R14 and R13.
const AARCH32_REG_PC: usize = 15; // R15
const AARCH32_REG_LR: usize = 14; // R14
const AARCH32_REG_SP: usize = 13; // R13
// Size of the 33 u64 general registers (r0-r29, lr, sp, pc) at the start of
// both the profiler's register block and `zx_restricted_state_t`.
const REGS_BYTES: usize = 33 * 8;
#[cfg(target_arch = "aarch64")]
const GUEST_ASPACE_BASE: u64 = starnix_uapi::restricted_aspace::RESTRICTED_ASPACE_BASE as u64;
const GUEST_ASPACE_END: u64 = RESTRICTED_ASPACE_HIGHEST_ADDRESS as u64;
#[cfg(target_arch = "aarch64")]
const RESTRICTED_STATE_SIZE: usize = std::mem::size_of::<zx_restricted_state_t>();
#[cfg(target_arch = "aarch64")]
const RESTRICTED_STATE_CPSR_OFFSET: usize = std::mem::offset_of!(zx_restricted_state_t, cpsr);
#[cfg(target_arch = "aarch64")]
const _: () = {
    assert!(std::mem::offset_of!(zx_restricted_state_t, sp) == AARCH64_REG_SP * 8);
    assert!(std::mem::offset_of!(zx_restricted_state_t, pc) == AARCH64_REG_PC * 8);
    assert!(REGS_BYTES <= RESTRICTED_STATE_CPSR_OFFSET);
};

mod event;
pub use event::{TraceEvent, TraceEventQueue, TraceEventQueueList};

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
struct LostRecord {
    header: perf_event_header,
    sample_id: u64,
    lost_events: u64,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
struct PerfMetadataHeader {
    version: u32,
    compat_version: u32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
struct PerfMetadataValue {
    lock: u32,
    index: u32,
    offset: i64,
    time_enabled: u64,
    time_running: u64,
    __bindgen_anon_1: perf_event_mmap_page__bindgen_ty_1,
    pmc_width: u16,
    time_shift: u16,
    time_mult: u32,
    time_offset: u64,
    time_zero: u64,
    size: u32,
    __reserved_1: u32,
    time_cycles: u64,
    time_mask: u64,
    __reserved: [u8; 928usize],
    data_head: u64,
    data_tail: u64,
    data_offset: u64,
    data_size: u64,
    aux_head: u64,
    aux_tail: u64,
    aux_offset: u64,
    aux_size: u64,
}

// SAFETY: `PerfMetadataValue` can be safely written to shared memory in 8-byte chunks.
// This is because it is composed of two u32s followed by only u64s.
// The first u32 is the `lock` field, which is why HAS_INLINE_SEQUENCE is true.
unsafe impl SeqLockable for PerfMetadataValue {
    const WRITE_SIZE: WriteSize = WriteSize::Eight;
    const HAS_INLINE_SEQUENCE: bool = true;
    const VMO_NAME: &'static [u8] = b"starnix:perf_event";
}

struct PerfState {
    // This table maps a group leader's file object id to its unique u64 "format ID".
    //
    // When a sample is generated for any event in a group, we use this
    // "format ID" from the group leader as the value for *both* the
    // `PERF_SAMPLE_ID` and `PERF_SAMPLE_IDENTIFIER` fields.
    format_id_lookup_table: LockDepMutex<HashMap<FileObjectId, u64>, PerfFormatIdLookupTableLock>,
}

impl Default for PerfState {
    fn default() -> Self {
        Self { format_id_lookup_table: Default::default() }
    }
}

fn get_perf_state(kernel: &Arc<Kernel>) -> Arc<PerfState> {
    kernel.expando.get_or_init(PerfState::default)
}

uapi::check_arch_independent_layout! {
    perf_event_attr {
        type_, // "type" is a reserved keyword so add a trailing underscore.
        size,
        config,
        __bindgen_anon_1,
        sample_type,
        read_format,
        _bitfield_1,
        __bindgen_anon_2,
        bp_type,
        __bindgen_anon_3,
        __bindgen_anon_4,
        branch_sample_type,
        sample_regs_user,
        sample_stack_user,
        clockid,
        sample_regs_intr,
        aux_watermark,
        sample_max_stack,
        __reserved_2,
        aux_sample_size,
        __reserved_3,
        sig_data,
        config3,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum IoctlOp {
    Enable,
    Disable,
}

struct PerfEventFileState {
    attr: perf_event_attr,
    rf_value: u64, // "count" for the config we passed in for the event.
    // The most recent timestamp (ns) where we changed into an enabled state
    // i.e. the most recent time we got an ENABLE ioctl().
    most_recent_enabled_time: u64,
    // Sum of all previous enablement segment durations (ns). If we are
    // currently in an enabled state, explicitly does NOT include the current
    // segment.
    total_time_running: u64,
    rf_id: u64,
    sample_id: u64,
    _rf_lost: u64,
    disabled: u64,
    sample_type: u64,
    // Handle to blob that stores all the perf data that a user may want.
    // At the moment it only stores some metadata and backtraces (bts).
    perf_data_vmo: zx::Vmo,
    // Channel used to send IoctlOps to start/stop sampling.
    ioctl_sender: future_mpsc::Sender<(IoctlOp, sync_mpsc::Sender<()>)>,
}

// Have an implementation for PerfEventFileState because VMO
// doesn't have Default so we can't derive it.
impl PerfEventFileState {
    fn new(
        attr: perf_event_attr,
        rf_value: u64,
        disabled: u64,
        sample_type: u64,
        perf_data_vmo: zx::Vmo,
        ioctl_sender: future_mpsc::Sender<(IoctlOp, sync_mpsc::Sender<()>)>,
    ) -> PerfEventFileState {
        PerfEventFileState {
            attr,
            rf_value,
            most_recent_enabled_time: 0,
            total_time_running: 0,
            rf_id: 0,
            sample_id: 0,
            _rf_lost: 0,
            disabled,
            sample_type,
            perf_data_vmo,
            ioctl_sender,
        }
    }
}

pub struct PerfEventFile {
    _tid: tid_t,
    _cpu: i32,
    perf_event_file: LockDepRwLock<PerfEventFileState, PerfEventLevel>,
    // The security state for this PerfEventFile.
    pub security_state: security::PerfEventState,
    seq_lock: Arc<OnceLock<Result<SeqLock<PerfMetadataHeader, PerfMetadataValue>, Errno>>>,
}

// PerfEventFile object that implements FileOps.
// See https://man7.org/linux/man-pages/man2/perf_event_open.2.html for
// implementation details.
// This object can be saved as a FileDescriptor.
impl FileOps for PerfEventFile {
    // Don't need to implement seek or sync for PerfEventFile.
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn close(self: Box<Self>, file: &FileObjectState, current_task: &CurrentTask) {
        {
            let mut perf_event_file = self.perf_event_file.write();
            // Ensure we disable so we clean up resources if the user closes without disabling.
            perf_event_file.disabled = 1;
            ping_receiver(perf_event_file.ioctl_sender.clone(), IoctlOp::Disable);
        }
        let perf_state = get_perf_state(&current_task.kernel);
        let mut events = perf_state.format_id_lookup_table.lock();
        events.remove(&file.id);
    }

    // See "Reading results" section of https://man7.org/linux/man-pages/man2/perf_event_open.2.html.
    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        // Create/calculate and return the ReadFormatData object.
        // If we create it earlier we might want to change it and it's immutable once created.
        let read_format_data = {
            // Once we get the `value` or count from kernel, we can change this to a read()
            // call instead of write().
            let mut perf_event_file = self.perf_event_file.write();

            security::check_perf_event_read_access(current_task, &self)?;

            let mut total_time_running_including_curr = perf_event_file.total_time_running;

            // Only update values if enabled (either by perf_event_attr or ioctl ENABLE call).
            if perf_event_file.disabled == 0 {
                // Calculate the value or "count" of the config we're interested in.
                // This value should reflect the value we are counting (defined in the config).
                // E.g. for PERF_COUNT_SW_CPU_CLOCK it would return the value from the CPU clock.
                // For now we just return rf_value + 1.
                track_stub!(
                    TODO("https://fxbug.dev/402938671"),
                    "[perf_event_open] implement read_format value"
                );
                perf_event_file.rf_value += 1;

                // Update time duration.
                let curr_time = zx::MonotonicInstant::get().into_nanos() as u64;
                total_time_running_including_curr +=
                    curr_time - perf_event_file.most_recent_enabled_time;
            }

            let mut output = Vec::<u8>::new();
            let value = perf_event_file.rf_value.to_ne_bytes();
            output.extend(value);

            let read_format = perf_event_file.attr.read_format;

            if (read_format & perf_event_read_format_PERF_FORMAT_TOTAL_TIME_ENABLED as u64) != 0 {
                // Total time (ns) event was enabled and running (currently same as TIME_RUNNING).
                output.extend(total_time_running_including_curr.to_ne_bytes());
            }
            if (read_format & perf_event_read_format_PERF_FORMAT_TOTAL_TIME_RUNNING as u64) != 0 {
                // Total time (ns) event was enabled and running (currently same as TIME_ENABLED).
                output.extend(total_time_running_including_curr.to_ne_bytes());
            }
            if (read_format & perf_event_read_format_PERF_FORMAT_ID as u64) != 0 {
                // Adds a 64-bit unique value that corresponds to the event group.
                output.extend(perf_event_file.rf_id.to_ne_bytes());
            }

            output
        };

        // The regular read() call allows the case where the bytes-we-want-to-read-in won't
        // fit in the output buffer. However, for perf_event_open's read(), "If you attempt to read
        // into a buffer that is not big enough to hold the data, the error ENOSPC results."
        if data.available() < read_format_data.len() {
            return error!(ENOSPC);
        }
        track_stub!(
            TODO("https://fxbug.dev/402453955"),
            "[perf_event_open] implement remaining error handling"
        );

        data.write(&read_format_data)
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        op: u32,
        _arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        track_stub!(
            TODO("https://fxbug.dev/405463320"),
            "[perf_event_open] implement PERF_IOC_FLAG_GROUP"
        );
        security::check_perf_event_write_access(current_task, &self)?;
        let mut perf_event_file = self.perf_event_file.write();
        match op {
            PERF_EVENT_IOC_ENABLE => {
                if perf_event_file.disabled != 0 {
                    perf_event_file.disabled = 0; // 0 = false.
                    perf_event_file.most_recent_enabled_time =
                        zx::MonotonicInstant::get().into_nanos() as u64;
                }

                // If we are sampling, invoke the profiler and collect a sample.
                // Currently this is an example sample collection.
                track_stub!(
                    TODO("https://fxbug.dev/398914921"),
                    "[perf_event_open] implement full sampling features"
                );
                if sampling_period(&perf_event_file.attr).is_some() {
                    ping_receiver(perf_event_file.ioctl_sender.clone(), IoctlOp::Enable);
                }
                return Ok(SUCCESS);
            }
            PERF_EVENT_IOC_DISABLE => {
                if perf_event_file.disabled == 0 {
                    perf_event_file.disabled = 1; // 1 = true.

                    // Update total_time_running now that the segment has ended.
                    let curr_time = zx::MonotonicInstant::get().into_nanos() as u64;
                    perf_event_file.total_time_running +=
                        curr_time - perf_event_file.most_recent_enabled_time;
                }
                if sampling_period(&perf_event_file.attr).is_some() {
                    ping_receiver(perf_event_file.ioctl_sender.clone(), IoctlOp::Disable);
                }
                return Ok(SUCCESS);
            }
            PERF_EVENT_IOC_RESET => {
                perf_event_file.rf_value = 0;
                return Ok(SUCCESS);
            }
            PERF_EVENT_IOC_REFRESH
            | PERF_EVENT_IOC_PERIOD
            | PERF_EVENT_IOC_SET_OUTPUT
            | PERF_EVENT_IOC_SET_FILTER
            | PERF_EVENT_IOC_ID
            | PERF_EVENT_IOC_SET_BPF
            | PERF_EVENT_IOC_PAUSE_OUTPUT
            | PERF_EVENT_IOC_MODIFY_ATTRIBUTES
            | PERF_EVENT_IOC_QUERY_BPF => {
                track_stub!(
                    TODO("https://fxbug.dev/404941053"),
                    "[perf_event_open] implement remaining ioctl() calls"
                );
                return error!(ENOSYS);
            }
            _ => error!(ENOTTY),
        }
    }

    // TODO(https://fxbug.dev/460245383) match behavior when mmap() is called multiple times.
    // Gets called when mmap() is called.
    // Immediately before sampling, this should get called by the user (e.g. the test
    // or Perfetto). We will then write the metadata to the VMO and return the pointer to it.
    fn get_memory(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        length: Option<usize>,
        _prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        let buffer_size: u64 = length.unwrap_or(0) as u64;
        let page_size = zx::system_get_page_size() as u64;
        if buffer_size <= page_size || buffer_size > ESTIMATED_MMAP_BUFFER_SIZE {
            return error!(EINVAL);
        }
        let data_size = buffer_size - page_size;
        if !data_size.is_power_of_two() {
            return error!(EINVAL);
        }

        self.seq_lock
            .get_or_init(|| {
                let perf_event_file = self.perf_event_file.read();
                let vmo_copy = perf_event_file
                    .perf_data_vmo
                    .as_handle_ref()
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .map_err(|status| from_status_like_fdio!(status))?;
                // SAFETY: See safety requirements on `create_seq_lock`.
                Ok(unsafe { create_seq_lock(&vmo_copy, buffer_size) })
            })
            .as_ref()
            .map_err(|e| e.clone())?;

        // Write to a MemoryObject and return it (expected return type for get_memory()).
        security::check_perf_event_read_access(current_task, &self)?;
        let perf_event_file = self.perf_event_file.read();
        match perf_event_file
            .perf_data_vmo
            .as_handle_ref()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
        {
            Ok(vmo) => {
                let vmo: zx::Vmo = vmo.into();
                let memory = MemoryObject::from(vmo);
                return Ok(Arc::new(memory));
            }
            Err(_) => {
                track_stub!(
                    TODO("https://fxbug.dev/416323134"),
                    "[perf_event_open] handle get_memory() errors"
                );
                return error!(EINVAL);
            }
        };
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        track_stub!(
            TODO("https://fxbug.dev/394960158"),
            "[perf_event_open] implement perf event functions"
        );
        error!(ENOSYS)
    }
}

// Given a PerfRecordSample struct, write it via the correct output format
// (per https://man7.org/linux/man-pages/man2/perf_event_open.2.html) to the VMO.
// We don't currently support all the sample_types listed in the docs.
// Input:
//    PerfRecordSample { pid: 5, tid: 10, nr: 3, ips[nr]: [111, 222, 333] }
// Human-understandable output:
//    9 1 40 111 5 10 3 111 222 333
// Actual output (no spaces or \n in real output, just making it more readable):
//    0x0000 0x0009                 <-- starts at `offset` bytes
//    0x0001
//    0x0040
//    0x0000 0x0000 0x0000 0x006F   <-- starts at `offset` + 8 bytes
//    0x0000 0x0000 0x0000 0x0005
//    0x0000 0x0000 0x0000 0x0010
//    0x0000 0x0000 0x0000 0x0003
//    0x0000 0x0000 0x0000 0x006F
//    0x0000 0x0000 0x0000 0x00DE
//    0x0000 0x0000 0x0000 0x014D
//
//    Returns the length of bytes written. In above case, 8 + 28 = 36.
//    This information is used to increment the global offset.
//
//    If writing to the VMO fails, we log a warning and return the number of
//    bytes successfully written so far (e.g. only the LOST record if that
//    succeeded, or 0). The caller uses this returned length to increment the
//    VMO write offset and update `data_head`, meaning failed writes are
//    effectively skipped and not exposed to the reader.
fn write_record_to_vmo(
    perf_record_sample: PerfRecordSample<'_>,
    perf_data_vmo: &zx::Vmo,
    sample_type: u64,
    sample_id: u64,
    sample_period: u64,
    read_format: u64,
    head: u64,
    metadata: &PerfMetadataValue,
    lost_events: &mut u64,
) -> u64 {
    let ring_buffer_size = metadata.data_size;
    if ring_buffer_size == 0 {
        return 0;
    }
    // First, build record to determine its size (so that we can fill out `size` in header).
    let mut sample = Vec::<u8>::new();
    // sample_id
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_IDENTIFIER as u64) != 0 {
        sample.extend(sample_id.to_ne_bytes());
    }
    // ip
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_IP as u64) != 0 {
        let ip = perf_record_sample.ips.first().copied().unwrap_or(0);
        sample.extend(ip.to_ne_bytes());
    }

    if (sample_type & perf_event_sample_format_PERF_SAMPLE_TID as u64) != 0 {
        // pid
        sample.extend(perf_record_sample.pid.unwrap_or(0).to_ne_bytes());
        // tid
        sample.extend(perf_record_sample.tid.unwrap_or(0).to_ne_bytes());
    }

    // time: when the sample was taken.
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_TIME as u64) != 0 {
        sample.extend((perf_record_sample.time.into_nanos() as u64).to_ne_bytes());
    }

    // id
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_ID as u64) != 0 {
        sample.extend(sample_id.to_ne_bytes());
    }

    // sample period
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_PERIOD as u64) != 0 {
        sample.extend(sample_period.to_ne_bytes());
    }

    // read_format value.
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_READ as u64) != 0 {
        if (read_format & perf_event_read_format_PERF_FORMAT_GROUP as u64) != 0 {
            // Group reads start with the number of events followed by each
            // event's value; only the timebase event exists.
            sample.extend(1u64.to_ne_bytes());
            sample.extend(0u64.to_ne_bytes());
        } else {
            sample.extend(0u64.to_ne_bytes());
        }
    }

    if (sample_type & perf_event_sample_format_PERF_SAMPLE_CALLCHAIN as u64) != 0 {
        // Callchains are structured into sections, each introduced by a
        // PERF_CONTEXT_* marker, and readers rely on these markers to split
        // kernel and user frames.
        //
        // Starnix normal-mode execution (>= GUEST_ASPACE_END, above
        // restricted_return_loop) represents kernel execution from the guest's
        // perspective and belongs in PERF_CONTEXT_KERNEL; restricted-mode
        // guest execution (< GUEST_ASPACE_END) belongs in PERF_CONTEXT_USER.
        // Both sections are always emitted; readers that unwind user stacks
        // themselves (DWARF) stop at the PERF_CONTEXT_USER marker.

        // Reserve nr, then fill it in once the markers and ips are written.
        let nr_offset = sample.len();
        sample.extend(0u64.to_ne_bytes());
        let callchain_start = sample.len();

        sample.extend(perf_callchain_context_PERF_CONTEXT_KERNEL.to_ne_bytes());
        for ip in perf_record_sample.ips.iter().filter(|&&ip| ip >= GUEST_ASPACE_END) {
            sample.extend(ip.to_ne_bytes());
        }
        sample.extend(perf_callchain_context_PERF_CONTEXT_USER.to_ne_bytes());
        for ip in perf_record_sample.ips.iter().filter(|&&ip| ip < GUEST_ASPACE_END) {
            sample.extend(ip.to_ne_bytes());
        }

        // nr, counting the markers and ips.
        let nr = ((sample.len() - callchain_start) / std::mem::size_of::<u64>()) as u64;
        sample[nr_offset..callchain_start].copy_from_slice(&nr.to_ne_bytes());
    }

    // User registers (PERF_SAMPLE_REGS_USER): the ABI tag, then one u64 per
    // bit set in attr.sample_regs_user, in perf register-index order.
    // Readers request the mask for their own architecture -- a 64-bit reader
    // requests the arm64 mask even when profiling 32-bit tasks (and relocates
    // the PC from the PERF_REG_ARM64_PC index itself, matching the Linux
    // kernel's behavior of capturing native registers).
    // If the mask is 0 or the ABI is PERF_SAMPLE_REGS_ABI_NONE, no register
    // values are output.
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_REGS_USER as u64) != 0 {
        if perf_record_sample.regs_abi == perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64
            || perf_record_sample.sample_regs_user == 0
            || perf_record_sample.regs.is_empty()
        {
            sample.extend((perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64).to_ne_bytes());
        } else {
            sample.extend(perf_record_sample.regs_abi.to_ne_bytes());
            sample.extend_from_slice(perf_record_sample.regs);
        }
    }

    // User stack (PERF_SAMPLE_STACK_USER): size, the raw bytes starting at
    // the sampled stack pointer, then the filled size. Readers overlay these
    // bytes at the SP reported in REGS_USER, which is why the snapshot must
    // begin exactly at it.
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_STACK_USER as u64) != 0 {
        let fixed_len = std::mem::size_of::<perf_event_header>() + sample.len() + 16;
        let header_space = (u16::MAX as usize).saturating_sub(fixed_len) & !7;
        let requested = (perf_record_sample.sample_stack_user as usize) & !7;
        let dest_len = requested.min(header_space);

        sample.extend((dest_len as u64).to_ne_bytes());
        if dest_len > 0 {
            let data_len = perf_record_sample.stack.len().min(dest_len) & !7;
            sample.extend_from_slice(&perf_record_sample.stack[..data_len]);
            // Pad the rest with zeros.
            sample.resize(sample.len() + (dest_len - data_len), 0);
            // dyn_size
            sample.extend((data_len as u64).to_ne_bytes());
        }
    }
    // The remaining sample_type fields are not implemented.

    // Now that we know the sample size, we can calculate the record size.
    // record_size = perf_event_header_size + sample_size.
    // perf_event_header is defined to be 8 bytes.
    let record_len = std::mem::size_of::<perf_event_header>() + sample.len();
    // Every field above is a u64 (or a u32 pair), so record sizes are always
    // multiples of 8: ring positions stay 8-byte aligned. Since the
    // perf_event_header is also 8 bytes, it can never straddle the end of
    // the data area (which is page-aligned), meaning readers can always
    // read the header contiguously.
    if record_len % 8 != 0 {
        log_error!("Record length {} is not 8-byte aligned, dropping", record_len);
        *lost_events += 1;
        return 0;
    }
    // The header's size field is a u16. A record that exceeds it would
    // silently wrap the size and desynchronize every record after it, so
    // drop it instead.
    let Ok(record_size) = u16::try_from(record_len) else {
        log_warn!("Dropping {} byte perf sample record: exceeds u16 record size", record_len);
        *lost_events += 1;
        return 0;
    };

    let perf_event_header = perf_event_header {
        // These are samples of user-space execution; readers take the
        // sample's cpu mode from the misc bits.
        type_: perf_event_type_PERF_RECORD_SAMPLE,
        misc: PERF_RECORD_MISC_USER as u16,
        size: record_size,
    };

    // data_tail is advanced by userspace as it consumes records and is untrusted.
    // Calculate free space using the standard circular buffer formula, matching Linux's
    // CIRC_SPACE. This naturally handles wrap-around and invalid future tails.
    // ring_buffer_size is guaranteed to be a power of two by checks in get_memory().
    let free_space =
        (metadata.data_tail.wrapping_sub(head).wrapping_sub(1)) & (ring_buffer_size - 1);

    // Drop the sample if the ring buffer is full, matching Linux's
    // non-overwrite mode; the drop is reported via PERF_RECORD_LOST once
    // space frees up.
    if free_space < record_len as u64 {
        *lost_events += 1;
        return 0;
    }

    let mut bytes_written: u64 = 0;

    // If records were dropped earlier, surface a PERF_RECORD_LOST record as
    // soon as there is room for it alongside the current sample.
    if *lost_events > 0 && free_space >= record_len as u64 + LOST_RECORD_SIZE {
        let lost_header = perf_event_header {
            type_: perf_event_type_PERF_RECORD_LOST,
            misc: 0,
            size: LOST_RECORD_SIZE as u16,
        };
        let lost_record = LostRecord { header: lost_header, sample_id, lost_events: *lost_events };
        if write_circular(
            perf_data_vmo,
            metadata.data_offset,
            ring_buffer_size,
            head,
            lost_record.as_bytes(),
        )
        .is_ok()
        {
            *lost_events = 0;
            bytes_written += LOST_RECORD_SIZE;
        }
    }

    let mut record = Vec::with_capacity(record_len);
    record.extend_from_slice(perf_event_header.as_bytes());
    record.extend_from_slice(&sample);

    match write_circular(
        perf_data_vmo,
        metadata.data_offset,
        ring_buffer_size,
        head + bytes_written,
        &record,
    ) {
        // Return the total size we wrote so the caller can advance data_head.
        Ok(()) => bytes_written + record_len as u64,
        Err(e) => {
            log_warn!("Failed to write PerfRecordSample to VMO due to: {}", e);
            bytes_written
        }
    }
}

// Writes `data` into the ring buffer's data area at the position
// corresponding to `head` (a free-running count of bytes ever written),
// splitting the write across the end of the data area when it wraps around.
// Readers read records by checking the header size, reading contiguously,
// and wrapping around to the beginning of the data area if the record is split.
fn write_circular(
    vmo: &zx::Vmo,
    data_offset: u64,
    ring_buffer_size: u64,
    head: u64,
    data: &[u8],
) -> Result<(), zx::Status> {
    let position = head % ring_buffer_size;
    let vmo_offset = data_offset + position;
    if position + data.len() as u64 <= ring_buffer_size {
        vmo.write(data, vmo_offset)?;
    } else {
        let first_len = (ring_buffer_size - position) as usize;
        vmo.write(&data[..first_len], vmo_offset)?;
        vmo.write(&data[first_len..], data_offset)?;
    }
    Ok(())
}

/// Represents a PERF_RECORD_SAMPLE payload to be serialized into the VMO ring buffer.
/// Fields follow the perf ABI order: TID, TIME, ID, PERIOD, READ, CALLCHAIN, REGS_USER, STACK_USER.
#[derive(Debug, Clone)]
struct PerfRecordSample<'a> {
    pid: Option<u32>,
    tid: Option<u32>,
    // Timestamp of when the sample was taken, for PERF_SAMPLE_TIME.
    time: zx::BootInstant,
    // Instruction pointers (currently this is the address). First one is `ip` param.
    ips: Vec<u64>,
    regs: &'a [u8],
    stack: &'a [u8],
    // PERF_SAMPLE_REGS_ABI_32 (1) or PERF_SAMPLE_REGS_ABI_64 (2) for `regs`.
    regs_abi: u64,
    sample_regs_user: u64,
    sample_stack_user: u64,
}

async fn set_up_profiler(
    sample_period: zx::MonotonicDuration,
    want_stack_samples: bool,
) -> Result<(profiler::SessionProxy, fidl::AsyncSocket), Errno> {
    // Configuration for how we want to sample. Events that asked for register
    // and stack state (PERF_SAMPLE_REGS_USER / PERF_SAMPLE_STACK_USER) need
    // raw stack snapshots, which only the DWARF strategy captures. Events that
    // just want a callchain (PERF_SAMPLE_CALLCHAIN) are served by the
    // in-kernel sampler, which unwinds via frame pointers without suspending
    // the sampled thread.
    let strategy = if want_stack_samples {
        profiler::CallgraphStrategy::Dwarf
    } else {
        profiler::CallgraphStrategy::FramePointer
    };
    let sample = profiler::Sample {
        callgraph: Some(profiler::CallgraphConfig {
            strategy: Some(strategy),
            ..Default::default()
        }),
        ..Default::default()
    };

    let sampling_config = profiler::SamplingConfig {
        period: Some(sample_period.into_nanos() as u64),
        timebase: Some(profiler::Counter::PlatformIndependent(profiler::CounterId::Nanoseconds)),
        sample: Some(sample),
        ..Default::default()
    };

    track_stub!(
        TODO("https://fxbug.dev/398914921"),
        "[perf_event_open] allow for profiling system-wide not during tests"
    );
    let job = fuchsia_runtime::job_default();
    let koid = job.koid().map_err(|e| errno!(EINVAL, e.to_string()))?;
    let tasks = vec![
        // Should return ~1300 samples for 1000 millis.
        profiler::Task::Job(koid.raw_koid()),
    ];
    let targets = profiler::TargetConfig::Tasks(tasks);
    let config = profiler::Config {
        configs: Some(vec![sampling_config]),
        target: Some(targets),
        ..Default::default()
    };
    let (client, server) = fidl::Socket::create_stream();
    let configure = profiler::SessionConfigureRequest {
        output: Some(server),
        config: Some(config),
        ..Default::default()
    };

    let proxy = connect_to_protocol::<profiler::SessionMarker>()
        .context("Error connecting to Profiler protocol");
    let session_proxy: profiler::SessionProxy = match proxy {
        Ok(p) => p.clone(),
        Err(e) => return error!(EINVAL, e),
    };

    // Must configure before sampling start().
    let config_request = session_proxy.configure(configure).await;
    match config_request {
        Ok(_) => Ok((session_proxy, fidl::AsyncSocket::from_socket(client))),
        Err(e) => return error!(EINVAL, e),
    }
}

// Returns true if `cpsr` indicates AArch32 execution.
#[cfg(target_arch = "aarch64")]
fn cpsr_is_aarch32(cpsr: u64) -> bool {
    (cpsr & zx::sys::ZX_REG_CPSR_ARCH_32_MASK) == zx::sys::ZX_REG_CPSR_ARCH_32_MASK
}

#[cfg(not(target_arch = "aarch64"))]
fn cpsr_is_aarch32(_cpsr: u64) -> bool {
    false
}

// Searches `chunks` for the thread's `zx_restricted_state_t` capture and
// returns the saved guest registers (`x0..x30, sp, pc`) and `cpsr`.
//
// A chunk matches if it is page-aligned in the Starnix address space, has
// the size of `zx_restricted_state_t`, and holds a guest PC and SP within
// the restricted address space.
#[cfg(target_arch = "aarch64")]
fn guest_regs_from_restricted_state<'a>(chunks: &[(u64, &'a [u8])]) -> Option<(&'a [u8], u64)> {
    chunks.iter().find_map(|&(base, data)| {
        if base < GUEST_ASPACE_END || base % *PAGE_SIZE != 0 || data.len() != RESTRICTED_STATE_SIZE
        {
            return None;
        }
        let read_reg =
            |index: usize| u64::from_ne_bytes(data[index * 8..index * 8 + 8].try_into().unwrap());
        let cpsr = u32::from_ne_bytes(
            data[RESTRICTED_STATE_CPSR_OFFSET..RESTRICTED_STATE_CPSR_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let cpsr = u64::from(cpsr);
        // AArch32 execution keeps its stack pointer in R13 (x13).
        let sp =
            if cpsr_is_aarch32(cpsr) { read_reg(AARCH32_REG_SP) } else { read_reg(AARCH64_REG_SP) };
        let pc = read_reg(AARCH64_REG_PC);
        if !(GUEST_ASPACE_BASE..GUEST_ASPACE_END).contains(&pc) || sp >= GUEST_ASPACE_END {
            return None;
        }
        Some((&data[..REGS_BYTES], cpsr))
    })
}

#[cfg(not(target_arch = "aarch64"))]
fn guest_regs_from_restricted_state<'a>(_chunks: &[(u64, &'a [u8])]) -> Option<(&'a [u8], u64)> {
    None
}

// Converts one FXT record from the profiler into a PERF_RECORD_SAMPLE in
// the ring buffer and publishes the new data_head. Non-sample records are
// ignored.
fn process_fxt_record(
    record: TraceRecord,
    seq_lock_wrapper: &SeqLock<PerfMetadataHeader, PerfMetadataValue>,
    perf_data_vmo: &zx::Vmo,
    sample_type: u64,
    sample_id: u64,
    sample_period: u64,
    read_format: u64,
    sample_regs_user: u64,
    sample_stack_user: u64,
    koid_session: Option<&PidKoidSession>,
    vmo_write_offset: &mut u64,
    lost_events: &mut u64,
) {
    match record {
        TraceRecord::Profiler(ProfilerRecord::Backtrace(backtrace)) => {
            let ips: Vec<u64> = backtrace.data;
            // Resolve the sampled koids to Linux pid/tid against the live
            // shared map (one read lock per record; collection is off the hot
            // path and a live read sees every thread that recorded itself
            // before it was sampled). If the sample cannot be resolved (e.g.
            // native Fuchsia thread or without a session), drop the sample.
            let Some(LinuxIdentity::Thread { pid, tid }) = koid_session.and_then(|s| {
                s.resolve_koids(
                    zx::Koid::from_raw(backtrace.process.0),
                    zx::Koid::from_raw(backtrace.thread.0),
                )
            }) else {
                return;
            };
            let time = zx::BootInstant::from_nanos(backtrace.timestamp.max(0));
            let perf_record_sample = PerfRecordSample {
                pid: Some(pid as u32),
                tid: Some(tid as u32),
                time,
                ips,
                regs: &[],
                stack: &[],
                regs_abi: perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64,
                sample_regs_user,
                sample_stack_user,
            };
            let metadata = seq_lock_wrapper.get();
            let bytes_written = write_record_to_vmo(
                perf_record_sample,
                perf_data_vmo,
                sample_type,
                sample_id,
                sample_period,
                read_format,
                *vmo_write_offset,
                &metadata,
                lost_events,
            );
            // Publish data_head after writing; set_value's
            // release-ordered stores make the record contents
            // visible to a reader that observes the new head.
            if bytes_written > 0 {
                *vmo_write_offset += bytes_written;
                let mut metadata = seq_lock_wrapper.get();
                metadata.data_head = *vmo_write_offset;
                seq_lock_wrapper.set_value(metadata);
            }
        }
        TraceRecord::LargeBlob(large_blob) => {
            // The DWARF strategy delivers each sample as a
            // "stack_sample" blob: [u64 regs_size][regs bytes]
            // followed by memory chunks of [u64 base][u64 size]
            // [bytes] (see the profiler's StackSampler).
            if large_blob.name != "stack_sample" {
                return;
            }
            let Some(blob_metadata) = large_blob.metadata else {
                return;
            };
            let bytes = &large_blob.bytes;
            if bytes.len() < 8 {
                return;
            }
            let regs_size = u64::from_ne_bytes(bytes[0..8].try_into().unwrap()) as usize;
            let mut offset = 8;
            if regs_size == 0 || bytes.len() < offset + regs_size {
                return;
            }

            // 33 u64 general registers (r0-r29, lr, sp, pc), with
            // cpsr following them in the zircon thread state.
            // Register state of other widths (e.g. an x86_64
            // thread state) is not supported and skipped by this
            // size check.
            if regs_size < REGS_BYTES + 8 {
                return;
            }
            // The size check above guarantees the 8 cpsr bytes are present.
            let thread_cpsr = u64::from_ne_bytes(
                bytes[offset + REGS_BYTES..offset + REGS_BYTES + 8].try_into().unwrap(),
            );
            let thread_regs = &bytes[offset..offset + REGS_BYTES];
            offset += regs_size;

            // The memory captures that follow the registers: the
            // thread-state stack, then, for a thread sampled while
            // running Starnix code, the restricted-state struct and
            // (if it could be read) the stack at the guest SP.
            let mut chunks = Vec::<(u64, &[u8])>::new();
            while offset + 16 <= bytes.len() {
                let chunk_base = u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap());
                offset += 8;
                let chunk_size = u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap());
                offset += 8;
                if offset + chunk_size as usize > bytes.len() {
                    break;
                }
                chunks.push((chunk_base, &bytes[offset..offset + chunk_size as usize]));
                offset += chunk_size as usize;
            }

            // If the thread was sampled in Starnix (e.g. during a syscall),
            // report the guest registers saved in its restricted state rather
            // than Starnix's own registers. If no valid restricted-state
            // chunk is present, fall back to the thread registers.
            let pc_offset = AARCH64_REG_PC * 8;
            let thread_pc =
                u64::from_ne_bytes(thread_regs[pc_offset..pc_offset + 8].try_into().unwrap());
            let in_starnix = thread_pc >= GUEST_ASPACE_END;
            let guest_state =
                if in_starnix { guest_regs_from_restricted_state(&chunks) } else { None };
            let reports_guest_state = guest_state.is_some();
            let (regs, cpsr) = guest_state.unwrap_or((thread_regs, thread_cpsr));
            let is_32bit = cpsr_is_aarch32(cpsr);

            // Borrow the register bytes directly; only AArch32 needs a
            // mutable copy to populate the sp/lr/r15 slots below.
            let mut blob_regs = Cow::Borrowed(regs);
            if is_32bit {
                let blob_regs = blob_regs.to_mut();
                // Zircon reports the AArch32 PC in the pc slot
                // (index 32). 64-bit readers take it from there,
                // per the Linux compat layout (AArch32 R0-R14
                // arrive in x0-x14); mirror it into the arm32 R15
                // slot (index 15) too for 32-bit readers, which
                // only consume indices 0-15 -- x15 carries no
                // meaningful value for AArch32 state.
                let r15_offset = AARCH32_REG_PC * 8;
                blob_regs.copy_within(pc_offset..pc_offset + 8, r15_offset);
                // Likewise, AArch32 execution keeps its stack pointer
                // and link register in R13 and R14; the sp and lr
                // slots hold SP_EL0 and X30, which it never updates.
                // Report R13/R14 in the sp/lr slots, as Linux does for
                // compat tasks (compat_sp / compat_lr).
                let r13_offset = AARCH32_REG_SP * 8;
                let sp_offset = AARCH64_REG_SP * 8;
                blob_regs.copy_within(r13_offset..r13_offset + 8, sp_offset);
                let r14_offset = AARCH32_REG_LR * 8;
                let lr_offset = AARCH64_REG_LR * 8;
                blob_regs.copy_within(r14_offset..r14_offset + 8, lr_offset);
            }

            let read_reg = |index: usize| {
                u64::from_ne_bytes(blob_regs[index * 8..index * 8 + 8].try_into().unwrap())
            };
            let sp = if is_32bit {
                // The arm32 stack pointer is R13.
                read_reg(AARCH32_REG_SP)
            } else {
                read_reg(AARCH64_REG_SP)
            };
            let pc = read_reg(AARCH64_REG_PC);

            // Select the memory chunk that contains the sampled
            // stack pointer and trim it to start exactly there:
            // readers overlay the STACK_USER bytes at the SP
            // reported in REGS_USER. The blob can carry several
            // captures (the thread-state stack, the
            // restricted-state struct, and a restricted-SP
            // stack); selecting by SP keeps the registers and the
            // stack bytes coherent. If no chunk covers the SP, emit
            // the sample with an empty stack.
            let stack: &[u8] = chunks
                .iter()
                .find_map(|&(chunk_base, data)| {
                    (chunk_base <= sp && sp - chunk_base < data.len() as u64)
                        .then(|| &data[(sp - chunk_base) as usize..])
                })
                .unwrap_or(&[]);

            let Some(LinuxIdentity::Thread { pid, tid }) = koid_session.and_then(|s| {
                s.resolve_koids(
                    zx::Koid::from_raw(blob_metadata.process.0),
                    zx::Koid::from_raw(blob_metadata.thread.0),
                )
            }) else {
                return;
            };
            let time = zx::BootInstant::from_nanos(blob_metadata.timestamp.max(0));
            let regs_abi = if is_32bit {
                perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32 as u64
            } else {
                perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64
            };
            let perf_record_sample = PerfRecordSample {
                pid: Some(pid as u32),
                tid: Some(tid as u32),
                time,
                // The Starnix PC (if any) goes to the kernel section
                // of the callchain, the user PC to the user section.
                ips: if reports_guest_state { vec![thread_pc, pc] } else { vec![pc] },
                regs: &blob_regs,
                stack,
                regs_abi,
                sample_regs_user,
                sample_stack_user,
            };
            let metadata = seq_lock_wrapper.get();
            let bytes_written = write_record_to_vmo(
                perf_record_sample,
                perf_data_vmo,
                sample_type,
                sample_id,
                sample_period,
                read_format,
                *vmo_write_offset,
                &metadata,
                lost_events,
            );
            // Publish data_head after writing; set_value's
            // release-ordered stores make the record contents
            // visible to a reader that observes the new head.
            if bytes_written > 0 {
                *vmo_write_offset += bytes_written;
                let mut metadata = seq_lock_wrapper.get();
                metadata.data_head = *vmo_write_offset;
                seq_lock_wrapper.set_value(metadata);
            }
        }
        _ => {}
    }
}

/// Returns the sampling period requested by `attr`, or `None` if this is not a
/// sampling event. `attr` carries either a frequency (Hz) or a period in the
/// same union, selected by the `freq` bit; the timebase counts nanoseconds.
fn sampling_period(attr: &perf_event_attr) -> Option<zx::MonotonicDuration> {
    // SAFETY: Reading a union field requires its bytes to be initialized and to
    // be a valid value for the field's type. `attr` is filled in by `read_object`
    // (or safe/Default construction), and this union's only fields, `sample_period`
    // and `sample_freq`, are both 8-byte `u64`s, so all 8 bytes are initialized
    // regardless of which field was written. Any 8 bytes are a valid `u64`, so
    // this read is sound.
    let value = unsafe { attr.__bindgen_anon_1.sample_period };
    if value == 0 {
        return None;
    }
    let nanos = if attr.freq() != 0 { (1_000_000_000 / value).max(1) } else { value };
    Some(zx::MonotonicDuration::from_nanos(nanos as i64))
}

// Notifies other thread that we should start/stop sampling.
// Once sampling is complete, that profiler session is no longer needed.
// At that point, send back notification so that this is no longer blocking
// (e.g. so that other profiler sessions can start).
fn ping_receiver(
    mut ioctl_sender: future_mpsc::Sender<(IoctlOp, sync_mpsc::Sender<()>)>,
    command: IoctlOp,
) {
    log_info!("[perf_event_open] Received sampling command: {:?}", command);
    let (profiling_complete_sender, profiling_complete_receiver) = sync_mpsc::channel::<()>();
    match ioctl_sender.try_send((command, profiling_complete_sender)) {
        Ok(_) => (),
        Err(e) => {
            if e.is_full() {
                log_warn!("[perf_event_open] Failed to send {:?}: Channel full", command);
            } else if e.is_disconnected() {
                log_warn!("[perf_event_open] Failed to send {:?}: Receiver disconnected", command);
            } else {
                log_warn!("[perf_event_open] Failed to send {:?} due to {:?}", command, e.source());
            }
        }
    };
    // Block on / wait until profiling is complete before returning.
    // This notifies that the profiler is free to be used for another session.
    let _ = profiling_complete_receiver.recv().unwrap();
}

// Creates a seq lock for the given VMO. Initializes the seq lock with
// known initial values (unknown values default to 0).
// Does NOT actually save this as a memory object until mmap() is called.
//
// # Safety
//
// The caller must ensure that the kernel maintains exclusive write access to this VMO and
// there are only atomic accesses to this memory (see seq_lock lib.rs for details).
unsafe fn create_seq_lock(
    vmo_handle_ref: &zx::NullableHandle,
    buffer_size: u64,
) -> SeqLock<PerfMetadataHeader, PerfMetadataValue> {
    // Currently we hardcode everything just to get something E2E working.
    let metadata_header = PerfMetadataHeader { version: 1, compat_version: 2 };
    let page_size = *PAGE_SIZE;
    let metadata_value = PerfMetadataValue {
        lock: 0,
        index: 3,
        offset: 19337,
        time_enabled: 0,
        time_running: 0,
        __bindgen_anon_1: perf_event_mmap_page__bindgen_ty_1 { capabilities: 30 },
        pmc_width: 0,
        time_shift: 0,
        time_mult: 0,
        time_offset: 0,
        time_zero: 0,
        size: 0,
        __reserved_1: 0,
        time_cycles: 0,
        time_mask: 0,
        __reserved: [0; 928usize],
        // This first page (metadata) has finished writing. Start data_head at 0.
        data_head: 0,
        // Start reading from 0; it is the user's responsibility to increment on their end.
        data_tail: 0,
        // We know the data will start after 1 page size so we can set this now.
        data_offset: page_size,
        data_size: buffer_size - page_size,
        aux_head: 0,
        aux_tail: 0,
        aux_offset: 0,
        aux_size: 0,
    };
    let vmo = zx::Vmo::from(vmo_handle_ref.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

    // Create a SeqLock and safely initialize the `header` and `value` for it.
    // SeqLock is formatted thusly:
    //   header_struct : any size, params `version` and `compat_version` should not change
    //   sequence_counter : u32, this is the lock and should increment
    //   value_struct : any size, each param can change
    //
    // SAFETY: See safety requirements on `create_seq_lock`.
    unsafe {
        SeqLock::new_from_vmo(metadata_header, metadata_value, vmo)
            .expect("failed to create seq_lock for perf metadata")
    }
}

pub fn sys_perf_event_open(
    current_task: &CurrentTask,
    attr: UserRef<perf_event_attr>,
    // Note that this is pid in Linux docs.
    tid: tid_t,
    cpu: i32,
    group_fd: FdNumber,
    _flags: u64,
) -> Result<SyscallResult, Errno> {
    // So far, the implementation only sets the read_data_format according to the "Reading results"
    // section of https://man7.org/linux/man-pages/man2/perf_event_open.2.html for a single event.
    // Other features will be added in the future (see below track_stubs).
    let perf_event_attrs: perf_event_attr = current_task.read_object(attr)?;

    if (perf_event_attrs.sample_type & perf_event_sample_format_PERF_SAMPLE_STACK_USER as u64) != 0
    {
        if perf_event_attrs.sample_stack_user % 8 != 0 {
            return error!(EINVAL);
        }
    }

    if tid == -1 && cpu == -1 {
        return error!(EINVAL);
    }

    let target = match tid {
        -1 => PerfEventTarget::AllTasks,
        0 => PerfEventTarget::CurrentTask,
        _ => {
            track_stub!(TODO("https://fxbug.dev/409621963"), "[perf_event_open] implement tid > 0");
            return error!(ENOSYS);
        }
    };
    let event_type: PerfEventType = perf_event_attrs.type_.try_into()?;

    // Check access to open any perf_event first, before any capability check, so that it is
    // evaluated and audited first.
    security::check_perf_event_open_access(current_task, PerfEventOpenType::Open)?;

    // Only require CAP_PERFMON (or CAP_SYS_ADMIN) when `perf_event_paranoid` exceeds the
    // threshold for the requested event.
    // TODO(https://fxbug.dev/398914921): Starnix currently starts a job-wide profiler session
    // regardless of `target`, so apply the CPU-wide threshold (`paranoid > 0`) to all targets
    // until sampling is scoped to the target task.
    let paranoid = current_task.kernel().perf_event_paranoid.load(Ordering::Relaxed);
    let requires_capability = (paranoid > 1 && perf_event_attrs.exclude_kernel() == 0)
        || paranoid > 0
        || (paranoid > -1 && event_type == PerfEventType::Tracepoint);
    // Exceptionally, a tracepoint perf event on the current task that excludes the kernel never
    // requires a capability. The kernel and CPU access checks below do not apply to such an event.
    let is_exempt = matches!(
        (event_type, perf_event_attrs.exclude_kernel(), &target),
        (PerfEventType::Tracepoint, 1, PerfEventTarget::CurrentTask)
    );
    // CAP_SYS_ADMIN is checked without auditing, so that nothing is audit-logged if the caller has
    // either capability, and only CAP_PERFMON is audit-logged if it has neither.
    if requires_capability
        && !is_exempt
        && !security::is_task_capable_noaudit(current_task, CAP_SYS_ADMIN)
    {
        security::check_task_capable(current_task, CAP_PERFMON).map_err(|_| errno!(EACCES))?;
    }

    // Check access to monitor the kernel when `exclude_kernel` is 0.
    if perf_event_attrs.exclude_kernel() == 0 {
        security::check_perf_event_open_access(current_task, PerfEventOpenType::Kernel)?;
    }

    // Check access to monitor CPU-wide activity when
    // - type is PERF_TYPE_SOFTWARE or
    // - type is PERF_TYPE_HARDWARE or
    // - type is in [CACHE, TRACEPOINT, BREAKPOINT, RAW) and pid == -1
    // - or when a low `perf_event_paranoid` waived the capability requirement
    //   (TODO(https://fxbug.dev/398914921): sampling is currently job-wide regardless of
    //   target task or event type).
    let check_cpu = !requires_capability
        || match event_type {
            PerfEventType::Software | PerfEventType::Hardware => true,
            PerfEventType::HwCache
            | PerfEventType::Tracepoint
            | PerfEventType::Breakpoint
            | PerfEventType::Raw => matches!(target, PerfEventTarget::AllTasks),
        };
    if check_cpu {
        security::check_perf_event_open_access(current_task, PerfEventOpenType::Cpu)?;
    }

    // Check access to monitor tracepoints when the type is PERF_TYPE_TRACEPOINT.
    if event_type == PerfEventType::Tracepoint {
        security::check_perf_event_open_access(current_task, PerfEventOpenType::Tracepoint)?;
    }

    // Channel used to send info between notifier and spawned task thread.
    // We somewhat arbitrarily picked 8 for now in case we get a bunch of ioctls that are in
    // quick succession (instead of something lower).
    let (sender, mut receiver) = future_mpsc::channel(8);

    let mut perf_event_file = PerfEventFileState::new(
        perf_event_attrs,
        0,
        perf_event_attrs.disabled(),
        perf_event_attrs.sample_type,
        zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap(),
        sender,
    );

    let read_format = perf_event_attrs.read_format;

    if (read_format & perf_event_read_format_PERF_FORMAT_TOTAL_TIME_ENABLED as u64) != 0
        || (read_format & perf_event_read_format_PERF_FORMAT_TOTAL_TIME_RUNNING as u64) != 0
    {
        // Only keep track of most_recent_enabled_time if we are currently in ENABLED state,
        // as otherwise this param shouldn't be used for calculating anything.
        if perf_event_file.disabled == 0 {
            perf_event_file.most_recent_enabled_time =
                zx::MonotonicInstant::get().into_nanos() as u64;
        }
        // Initialize this to 0 as we will need to return a time duration later during read().
        perf_event_file.total_time_running = 0;
    }

    let event_id = READ_FORMAT_ID_GENERATOR.fetch_add(1, Ordering::Relaxed);
    perf_event_file.rf_id = event_id;

    if group_fd.raw() == -1 {
        perf_event_file.sample_id = event_id;
    } else {
        let group_file = current_task.files().get(group_fd)?;
        let group_file_object_id = group_file.id;
        let perf_state = get_perf_state(&current_task.kernel);
        let events = perf_state.format_id_lookup_table.lock();
        if let Some(rf_id) = events.get(&group_file_object_id) {
            perf_event_file.sample_id = *rf_id;
        } else {
            return error!(EINVAL);
        }
    }

    if (read_format & perf_event_read_format_PERF_FORMAT_GROUP as u64) != 0 {
        track_stub!(
            TODO("https://fxbug.dev/402238049"),
            "[perf_event_open] implement read_format group"
        );
        return error!(ENOSYS);
    }
    if (read_format & perf_event_read_format_PERF_FORMAT_LOST as u64) != 0 {
        track_stub!(
            TODO("https://fxbug.dev/402260383"),
            "[perf_event_open] implement read_format lost"
        );
    }

    // Set up notifier for handling ioctl calls to enable/disable sampling.
    let mut vmo_handle_copy =
        perf_event_file.perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS);

    let sample_period = sampling_period(&perf_event_file.attr);

    // Events that request register and stack state need the profiler to
    // capture raw stacks (the DWARF strategy); callchain-only events are
    // unwound by the in-kernel sampler via frame pointers.
    let want_stack_samples = (perf_event_file.sample_type
        & (perf_event_sample_format_PERF_SAMPLE_REGS_USER as u64
            | perf_event_sample_format_PERF_SAMPLE_STACK_USER as u64))
        != 0;

    // SeqLock does not get instantiated with metadata values until mmap() is called.
    let seq_lock =
        Arc::new(OnceLock::<Result<SeqLock<PerfMetadataHeader, PerfMetadataValue>, Errno>>::new());
    let cloned_seq_lock = Arc::clone(&seq_lock);
    let mut vmo_write_offset = 0;

    let closure = async move |kthread_task: &CurrentTask| {
        let mut lost_events: u64 = 0;

        // Each iteration waits for an Enable and then runs one session.
        while let Some((command, profiling_complete_receiver)) = receiver.next().await {
            // We only expect Enable when no session is active.
            if command != IoctlOp::Enable {
                let _ = profiling_complete_receiver.send(());
                continue;
            }

            // Enable is only sent for sampling events, but guard anyway.
            let Some(sample_period) = sample_period else {
                let _ = profiling_complete_receiver.send(());
                continue;
            };

            let (session_proxy, client) =
                match set_up_profiler(sample_period, want_stack_samples).await {
                    Ok(session) => session,
                    Err(e) => {
                        log_warn!("Failed to profile: {}", e);
                        let _ = profiling_complete_receiver.send(());
                        continue;
                    }
                };

            // Record pid/koid mappings before the profiler starts sampling
            // so every sampled thread can be resolved. Dropping the session
            // at the end of the profiling session ends the recording interest.
            let pid_koid_session = kthread_task.kernel().trace_event_manager.open();
            let start_request =
                profiler::SessionStartRequest { buffer_results: Some(false), ..Default::default() };
            if let Err(e) = session_proxy.start(&start_request).await {
                log_warn!("Failed to start profiler: {:?}", e);
                let _ = profiling_complete_receiver.send(());
                continue;
            }
            let _ = profiling_complete_receiver.send(());

            let vmo = zx::Vmo::from(
                vmo_handle_copy
                    .as_mut()
                    .expect("Failed to get VMO handle")
                    .as_handle_ref()
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .unwrap(),
            );
            let mut handle_record = |record: TraceRecord| {
                // Records can only be written once mmap() has set up the
                // ring buffer.
                if let Some(Ok(seq_lock_wrapper)) = cloned_seq_lock.get() {
                    process_fxt_record(
                        record,
                        seq_lock_wrapper,
                        &vmo,
                        perf_event_file.sample_type,
                        perf_event_file.sample_id,
                        sample_period.into_nanos() as u64,
                        perf_event_file.attr.read_format,
                        perf_event_file.attr.sample_regs_user,
                        perf_event_file.attr.sample_stack_user as u64,
                        Some(&pid_koid_session),
                        &mut vmo_write_offset,
                        &mut lost_events,
                    );
                }
            };

            // Pump records from the profiler into the ring buffer until a
            // Disable arrives: readers poll the ring during the session, and
            // the profiler's socket must be drained continuously.
            let (stream, _parser_task) = SessionParser::new_async(client);
            let mut stream = stream.fuse();
            let mut stream_ended = false;
            let disable_ack = loop {
                if stream_ended {
                    // The profiler closed the socket early; only commands
                    // remain.
                    match receiver.next().await {
                        Some((IoctlOp::Disable, ack)) => break Some(ack),
                        Some((IoctlOp::Enable, ack)) => {
                            // A session is already active.
                            let _ = ack.send(());
                        }
                        None => break None,
                    }
                } else {
                    futures::select_biased! {
                        cmd = receiver.next() => match cmd {
                            Some((IoctlOp::Disable, ack)) => break Some(ack),
                            Some((IoctlOp::Enable, ack)) => {
                                // A session is already active.
                                let _ = ack.send(());
                            }
                            None => break None,
                        },
                        record = stream.next() => match record {
                            Some(Ok(record)) => handle_record(record),
                            Some(Err(e)) => {
                                // The stream is desynchronized and the parser
                                // would keep returning this error, so end the
                                // session. The teardown below drops the
                                // socket, which unblocks the profiler if it is
                                // stalled writing to it.
                                log_warn!("[perf_event_open] Error parsing FXT: {:?}", e);
                                stream_ended = true;
                                break None;
                            }
                            None => {
                                log_warn!("[perf_event_open] Profiler stream ended mid-session");
                                stream_ended = true;
                            }
                        },
                    }
                }
            };

            // Tear the session down while still draining: the profiler is a
            // separate process whose socket writes block, so if its socket is
            // full it is stalled mid-write and cannot service stop() or
            // reset() until we keep reading. Awaiting either without pumping
            // would deadlock the two.
            let stop_and_reset = async {
                match session_proxy.stop().await {
                    Ok(stats) => log_info!(
                        "[perf_event_open] profiler samples_collected: {:?}",
                        stats.samples_collected.unwrap_or(0)
                    ),
                    Err(e) => log_warn!("[perf_event_open] Failed to stop profiler: {:?}", e),
                }
                // Reset flushes the remaining data and closes the socket,
                // which is what ends the drain below.
                let _ = session_proxy.reset().await;
            };
            let drain = async move {
                if !stream_ended {
                    while let Some(record) = stream.next().await {
                        match record {
                            Ok(record) => handle_record(record),
                            Err(e) => {
                                // The stream is desynchronized, so further
                                // records cannot be parsed.
                                log_warn!("[perf_event_open] Error parsing FXT: {:?}", e);
                                break;
                            }
                        }
                    }
                }
                // Hand the profiler a closed socket so that a write it is
                // blocked on fails instead of hanging forever.
                drop(stream);
                drop(_parser_task);
            };
            futures::join!(stop_and_reset, drain);

            // The Disable ioctl returns only after the drain above, so the
            // reader sees every record once the ioctl completes.
            if let Some(ack) = disable_ack {
                let _ = ack.send(());
            }
        }
    };
    let req = SpawnRequestBuilder::new()
        .with_debug_name("perf-event-sampler")
        .with_async_closure(closure)
        .build();
    current_task.kernel().kthreads.spawner().spawn_from_request(req);

    let file = Box::new(PerfEventFile {
        _tid: tid,
        _cpu: cpu,
        perf_event_file: perf_event_file.into(),
        security_state: security::perf_event_alloc(current_task),
        seq_lock: seq_lock,
    });
    // TODO: https://fxbug.dev/404739824 - Confirm whether to handle this as a "private" node.
    let file_handle = Anon::new_private_file(current_task, file, OpenFlags::RDWR, "[perf_event]");
    let file_object_id = file_handle.id;
    let file_descriptor: Result<FdNumber, Errno> =
        current_task.add_file(file_handle, FdFlags::empty());

    match file_descriptor {
        Ok(fd) => {
            if group_fd.raw() == -1 {
                let perf_state = get_perf_state(&current_task.kernel);
                let mut events = perf_state.format_id_lookup_table.lock();
                events.insert(file_object_id, event_id);
            }
            Ok(fd.into())
        }
        Err(_) => {
            track_stub!(
                TODO("https://fxbug.dev/402453955"),
                "[perf_event_open] implement remaining error handling"
            );
            error!(EMFILE)
        }
    }
}
// Syscalls for arch32 usage
#[cfg(target_arch = "aarch64")]
mod arch32 {
    pub use super::sys_perf_event_open as sys_arch32_perf_event_open;
}

#[cfg(target_arch = "aarch64")]
pub use arch32::*;

use crate::mm::memory::MemoryObject;
use crate::mm::{MemoryAccessorExt, ProtectionFlags};
use crate::task::CurrentTask;
use crate::vfs::{
    Anon, FdFlags, FdNumber, FileObject, FileObjectId, FileObjectState, FileOps, InputBuffer,
    OutputBuffer,
};
use crate::{fileops_impl_nonseekable, fileops_impl_noop_sync};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::tracing::{TracePerformanceEventManager, ZirconIdentity};

    #[::fuchsia::test]
    async fn test_process_fxt_record_resolves_pid_tid() {
        let manager = Arc::new(TracePerformanceEventManager::new(std::sync::Weak::new()));
        let session = manager.open();
        manager.record(
            42,
            43,
            ZirconIdentity { process: zx::Koid::from_raw(1001), thread: zx::Koid::from_raw(1002) },
        );

        let perf_data_vmo = zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap();
        let vmo_handle_copy =
            perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        // SAFETY: The test maintains exclusive write access to this VMO.
        let seq_lock = unsafe { create_seq_lock(&vmo_handle_copy, ESTIMATED_MMAP_BUFFER_SIZE) };

        let sample_type = (perf_event_sample_format_PERF_SAMPLE_IP
            | perf_event_sample_format_PERF_SAMPLE_TID) as u64;
        let mut vmo_write_offset = 0;
        let mut lost_events = 0;

        // Mapped sample: process 1001, thread 1002 -> should resolve to pid 42, tid 43.
        let mapped_record =
            TraceRecord::Profiler(ProfilerRecord::Backtrace(fxt::profiler::BacktraceRecord {
                timestamp: 1000,
                process: fxt::ProcessKoid(1001),
                thread: fxt::ThreadKoid(1002),
                num_records: 1,
                data: vec![0x12345678],
            }));
        process_fxt_record(
            mapped_record,
            &seq_lock,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            0,
            0,
            Some(&session),
            &mut vmo_write_offset,
            &mut lost_events,
        );

        // Header (8 bytes) + IP (8 bytes) + PID/TID (8 bytes) = 24 bytes.
        let expected_record_size: u64 = 24;
        assert_eq!(vmo_write_offset, expected_record_size);

        let metadata = seq_lock.get();
        assert_eq!(metadata.data_head, expected_record_size);

        let mut record_bytes = [0u8; 24];
        perf_data_vmo.read(&mut record_bytes, metadata.data_offset).unwrap();

        let record_type = u32::from_ne_bytes(record_bytes[0..4].try_into().unwrap());
        assert_eq!(record_type, perf_event_type_PERF_RECORD_SAMPLE);

        let misc = u16::from_ne_bytes(record_bytes[4..6].try_into().unwrap());
        assert_eq!(misc, PERF_RECORD_MISC_USER as u16);

        let size = u16::from_ne_bytes(record_bytes[6..8].try_into().unwrap());
        assert_eq!(size, expected_record_size as u16);

        let ip = u64::from_ne_bytes(record_bytes[8..16].try_into().unwrap());
        assert_eq!(ip, 0x12345678);

        let pid = u32::from_ne_bytes(record_bytes[16..20].try_into().unwrap());
        assert_eq!(pid, 42);

        let tid = u32::from_ne_bytes(record_bytes[20..24].try_into().unwrap());
        assert_eq!(tid, 43);

        // Unmapped sample: process 9999, thread 9998 -> should be dropped.
        let unmapped_record =
            TraceRecord::Profiler(ProfilerRecord::Backtrace(fxt::profiler::BacktraceRecord {
                timestamp: 2000,
                process: fxt::ProcessKoid(9999),
                thread: fxt::ThreadKoid(9998),
                num_records: 1,
                data: vec![0x87654321],
            }));
        process_fxt_record(
            unmapped_record,
            &seq_lock,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            0,
            0,
            Some(&session),
            &mut vmo_write_offset,
            &mut lost_events,
        );

        // Verify that unmapped sample was dropped and no extra bytes were written.
        assert_eq!(vmo_write_offset, expected_record_size);
        let metadata = seq_lock.get();
        assert_eq!(metadata.data_head, expected_record_size);
        let mut trailing_bytes = [0u8; 24];
        perf_data_vmo
            .read(&mut trailing_bytes, metadata.data_offset + expected_record_size)
            .unwrap();
        assert_eq!(trailing_bytes, [0u8; 24]);
    }

    #[::fuchsia::test]
    async fn test_write_record_to_vmo_regs_and_stack() {
        let perf_data_vmo = zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap();
        let vmo_handle_copy =
            perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        // SAFETY: The test maintains exclusive write access to this VMO.
        let seq_lock = unsafe { create_seq_lock(&vmo_handle_copy, ESTIMATED_MMAP_BUFFER_SIZE) };
        let metadata = seq_lock.get();
        let mut lost_events = 0;
        let regs_data = [0x11u8; 16];
        let stack_data = [0x22u8; 16];
        let sample = PerfRecordSample {
            pid: Some(10),
            tid: Some(20),
            time: zx::BootInstant::from_nanos(123_456_789),
            ips: vec![0xdeadbeef],
            regs: &regs_data,
            stack: &stack_data,
            regs_abi: perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32 as u64,
            sample_regs_user: 3,
            sample_stack_user: 16,
        };
        let sample_type = (perf_event_sample_format_PERF_SAMPLE_IP
            | perf_event_sample_format_PERF_SAMPLE_TID
            | perf_event_sample_format_PERF_SAMPLE_TIME
            | perf_event_sample_format_PERF_SAMPLE_REGS_USER
            | perf_event_sample_format_PERF_SAMPLE_STACK_USER) as u64;
        let written = write_record_to_vmo(
            sample,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            0,
            &metadata,
            &mut lost_events,
        );
        assert!(written > 0);

        let mut record_bytes = vec![0u8; written as usize];
        perf_data_vmo.read(&mut record_bytes, metadata.data_offset).unwrap();

        let record_type = u32::from_ne_bytes(record_bytes[0..4].try_into().unwrap());
        assert_eq!(record_type, perf_event_type_PERF_RECORD_SAMPLE);
        let misc = u16::from_ne_bytes(record_bytes[4..6].try_into().unwrap());
        assert_eq!(misc, PERF_RECORD_MISC_USER as u16);
        let size = u16::from_ne_bytes(record_bytes[6..8].try_into().unwrap());
        assert_eq!(size as usize, written as usize);

        let ip = u64::from_ne_bytes(record_bytes[8..16].try_into().unwrap());
        assert_eq!(ip, 0xdeadbeef);

        let pid = u32::from_ne_bytes(record_bytes[16..20].try_into().unwrap());
        assert_eq!(pid, 10);
        let tid = u32::from_ne_bytes(record_bytes[20..24].try_into().unwrap());
        assert_eq!(tid, 20);

        let time = u64::from_ne_bytes(record_bytes[24..32].try_into().unwrap());
        assert_eq!(time, 123_456_789);

        // REGS_USER: abi (8 bytes) + regs (16 bytes)
        let abi = u64::from_ne_bytes(record_bytes[32..40].try_into().unwrap());
        assert_eq!(abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32 as u64);
        assert_eq!(&record_bytes[40..56], &regs_data);

        // STACK_USER: dest_len (8 bytes) + data (16 bytes) + dyn_size (8 bytes)
        let stack_len = u64::from_ne_bytes(record_bytes[56..64].try_into().unwrap());
        assert_eq!(stack_len, 16);
        assert_eq!(&record_bytes[64..80], &stack_data);
        let dyn_size = u64::from_ne_bytes(record_bytes[80..88].try_into().unwrap());
        assert_eq!(dyn_size, 16);
    }

    #[::fuchsia::test]
    async fn test_write_record_to_vmo_regs_abi_64() {
        let perf_data_vmo = zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap();
        let vmo_handle_copy =
            perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        // SAFETY: The test maintains exclusive write access to this VMO.
        let seq_lock = unsafe { create_seq_lock(&vmo_handle_copy, ESTIMATED_MMAP_BUFFER_SIZE) };
        let metadata = seq_lock.get();
        let mut lost_events = 0;
        let regs_data = [0x44u8; 16];
        let stack_data = [0x55u8; 16];
        let sample = PerfRecordSample {
            pid: Some(10),
            tid: Some(20),
            time: zx::BootInstant::from_nanos(123_456_789),
            ips: vec![0xdeadbeef],
            regs: &regs_data,
            stack: &stack_data,
            regs_abi: perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64,
            sample_regs_user: 3,
            sample_stack_user: 16,
        };
        let sample_type = (perf_event_sample_format_PERF_SAMPLE_IP
            | perf_event_sample_format_PERF_SAMPLE_TID
            | perf_event_sample_format_PERF_SAMPLE_TIME
            | perf_event_sample_format_PERF_SAMPLE_REGS_USER
            | perf_event_sample_format_PERF_SAMPLE_STACK_USER) as u64;
        let written = write_record_to_vmo(
            sample,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            0,
            &metadata,
            &mut lost_events,
        );
        assert!(written > 0);

        let mut record_bytes = vec![0u8; written as usize];
        perf_data_vmo.read(&mut record_bytes, metadata.data_offset).unwrap();

        // REGS_USER: abi (8 bytes) + regs (16 bytes)
        let abi = u64::from_ne_bytes(record_bytes[32..40].try_into().unwrap());
        assert_eq!(abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        assert_eq!(&record_bytes[40..56], &regs_data);
    }

    #[::fuchsia::test]
    async fn test_write_record_to_vmo_regs_abi_none_when_empty() {
        let perf_data_vmo = zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap();
        let vmo_handle_copy =
            perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        // SAFETY: The test maintains exclusive write access to this VMO.
        let seq_lock = unsafe { create_seq_lock(&vmo_handle_copy, ESTIMATED_MMAP_BUFFER_SIZE) };
        let metadata = seq_lock.get();
        let mut lost_events = 0;
        let stack_data = [0x33u8; 8];
        let sample = PerfRecordSample {
            pid: Some(10),
            tid: Some(20),
            time: zx::BootInstant::from_nanos(999),
            ips: vec![0x1000],
            regs: &[],
            stack: &stack_data,
            regs_abi: perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64,
            sample_regs_user: 3,
            sample_stack_user: 8,
        };
        let sample_type = (perf_event_sample_format_PERF_SAMPLE_IP
            | perf_event_sample_format_PERF_SAMPLE_TID
            | perf_event_sample_format_PERF_SAMPLE_TIME
            | perf_event_sample_format_PERF_SAMPLE_REGS_USER
            | perf_event_sample_format_PERF_SAMPLE_STACK_USER) as u64;
        let written = write_record_to_vmo(
            sample,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            0,
            &metadata,
            &mut lost_events,
        );
        assert!(written > 0);

        let mut record_bytes = vec![0u8; written as usize];
        perf_data_vmo.read(&mut record_bytes, metadata.data_offset).unwrap();

        // REGS_USER: abi (8 bytes) should be PERF_SAMPLE_REGS_ABI_NONE (0), and no regs follow.
        let abi = u64::from_ne_bytes(record_bytes[32..40].try_into().unwrap());
        assert_eq!(abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64);

        // STACK_USER immediately follows the ABI tag at offset 40.
        let stack_len = u64::from_ne_bytes(record_bytes[40..48].try_into().unwrap());
        assert_eq!(stack_len, 8);
        assert_eq!(&record_bytes[48..56], &stack_data);
        let dyn_size = u64::from_ne_bytes(record_bytes[56..64].try_into().unwrap());
        assert_eq!(dyn_size, 8);
    }

    // Stack-sample (DWARF) blobs, laid out as the profiler's StackSampler
    // emits them: [u64 regs_size][thread-state regs][{u64 base, u64 size,
    // bytes}...].
    const STARNIX_PC: u64 = GUEST_ASPACE_END + 0x2222_0558;
    const STARNIX_SP: u64 = GUEST_ASPACE_END + 0x4000_1000;
    const RESTRICTED_STATE_ADDR: u64 = GUEST_ASPACE_END + 0x5000_0000;
    const GUEST_PC: u64 = 0x7a00_1234;
    const GUEST_SP: u64 = 0x7f00_1000;
    const ALL_REGS_MASK: u64 = (1 << 33) - 1;
    const SAMPLE_STACK_USER: u64 = 256;

    fn u64s_to_bytes(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_ne_bytes()).collect()
    }

    fn read_u64(bytes: &[u8], pos: &mut usize) -> u64 {
        let value = u64::from_ne_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
        *pos += 8;
        value
    }

    // An arm64 zx_thread_state_general_regs_t: r0-r29, lr, sp, pc, cpsr,
    // tpidr.
    fn thread_state_regs(pc: u64, sp: u64, cpsr: u64) -> Vec<u64> {
        let mut regs: Vec<u64> = (0..31).map(|i| 0xa000 + i).collect();
        regs.extend([sp, pc, cpsr, 0x7777]);
        regs
    }

    fn stack_sample_blob(thread_regs: &[u64], chunks: &[(u64, &[u8])]) -> Vec<u8> {
        let regs_bytes = u64s_to_bytes(thread_regs);
        let mut blob = (regs_bytes.len() as u64).to_ne_bytes().to_vec();
        blob.extend(regs_bytes);
        for (base, data) in chunks {
            blob.extend(base.to_ne_bytes());
            blob.extend((data.len() as u64).to_ne_bytes());
            blob.extend_from_slice(data);
        }
        blob
    }

    // A zx_restricted_state_t with x0-x30 from `x`, laid out per the
    // struct's field offsets.
    #[cfg(target_arch = "aarch64")]
    fn restricted_state_bytes(x: &[u64; 31], sp: u64, pc: u64, cpsr: u32) -> Vec<u8> {
        use zx::sys::zx_restricted_state_t;
        let mut state = vec![0u8; std::mem::size_of::<zx_restricted_state_t>()];
        state[..31 * 8].copy_from_slice(&u64s_to_bytes(x));
        let sp_offset = std::mem::offset_of!(zx_restricted_state_t, sp);
        state[sp_offset..sp_offset + 8].copy_from_slice(&sp.to_ne_bytes());
        let pc_offset = std::mem::offset_of!(zx_restricted_state_t, pc);
        state[pc_offset..pc_offset + 8].copy_from_slice(&pc.to_ne_bytes());
        let cpsr_offset = std::mem::offset_of!(zx_restricted_state_t, cpsr);
        state[cpsr_offset..cpsr_offset + 4].copy_from_slice(&cpsr.to_ne_bytes());
        state
    }

    fn stack_bytes(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    struct ParsedStackSample {
        ip: u64,
        pid: u32,
        tid: u32,
        callchain: Vec<u64>,
        abi: u64,
        regs: Vec<u64>,
        stack: Vec<u8>,
    }

    // Runs `blob` through process_fxt_record as a sample of process 1001 /
    // thread 1002 (pid 42 / tid 43) and parses the one record it writes.
    fn process_stack_sample(blob: Vec<u8>, sample_regs_user: u64) -> ParsedStackSample {
        let manager = Arc::new(TracePerformanceEventManager::new(std::sync::Weak::new()));
        let session = manager.open();
        manager.record(
            42,
            43,
            ZirconIdentity { process: zx::Koid::from_raw(1001), thread: zx::Koid::from_raw(1002) },
        );
        let perf_data_vmo = zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap();
        let vmo_handle_copy =
            perf_data_vmo.as_handle_ref().duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        // SAFETY: The test maintains exclusive write access to this VMO.
        let seq_lock = unsafe { create_seq_lock(&vmo_handle_copy, ESTIMATED_MMAP_BUFFER_SIZE) };
        let sample_type = (perf_event_sample_format_PERF_SAMPLE_IP
            | perf_event_sample_format_PERF_SAMPLE_TID
            | perf_event_sample_format_PERF_SAMPLE_CALLCHAIN
            | perf_event_sample_format_PERF_SAMPLE_REGS_USER
            | perf_event_sample_format_PERF_SAMPLE_STACK_USER) as u64;
        let record = TraceRecord::LargeBlob(fxt::LargeBlobRecord {
            provider: None,
            ty: fxt::BlobType::Data,
            category: "cpu_profiler".into(),
            name: "stack_sample".into(),
            bytes: blob,
            metadata: Some(fxt::LargeBlobMetadata {
                timestamp: 1000,
                process: fxt::ProcessKoid(1001),
                thread: fxt::ThreadKoid(1002),
                args: vec![],
            }),
        });
        let mut vmo_write_offset = 0;
        let mut lost_events = 0;
        process_fxt_record(
            record,
            &seq_lock,
            &perf_data_vmo,
            sample_type,
            0,
            0,
            0,
            sample_regs_user,
            SAMPLE_STACK_USER,
            Some(&session),
            &mut vmo_write_offset,
            &mut lost_events,
        );
        assert!(vmo_write_offset > 0, "no record written");
        assert_eq!(lost_events, 0);

        let mut record = vec![0u8; vmo_write_offset as usize];
        perf_data_vmo.read(&mut record, seq_lock.get().data_offset).unwrap();
        assert_eq!(
            u32::from_ne_bytes(record[0..4].try_into().unwrap()),
            perf_event_type_PERF_RECORD_SAMPLE
        );
        assert_eq!(u16::from_ne_bytes(record[6..8].try_into().unwrap()) as usize, record.len());

        let mut pos = 8;
        let ip = read_u64(&record, &mut pos);
        let pid = u32::from_ne_bytes(record[pos..pos + 4].try_into().unwrap());
        let tid = u32::from_ne_bytes(record[pos + 4..pos + 8].try_into().unwrap());
        pos += 8;
        let nr = read_u64(&record, &mut pos);
        let callchain = (0..nr).map(|_| read_u64(&record, &mut pos)).collect();
        let abi = read_u64(&record, &mut pos);
        let regs = if abi == perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_NONE as u64 {
            vec![]
        } else {
            (0..sample_regs_user.count_ones()).map(|_| read_u64(&record, &mut pos)).collect()
        };
        let stack_size = read_u64(&record, &mut pos) as usize;
        assert_eq!(stack_size, SAMPLE_STACK_USER as usize);
        let stack_region = record[pos..pos + stack_size].to_vec();
        pos += stack_size;
        let dyn_size = read_u64(&record, &mut pos) as usize;
        assert_eq!(pos, record.len());
        ParsedStackSample {
            ip,
            pid,
            tid,
            callchain,
            abi,
            regs,
            stack: stack_region[..dyn_size].to_vec(),
        }
    }

    // Checks that `sample` fell back to reporting the Starnix thread registers
    // and stack.
    fn assert_reports_thread_state(sample: &ParsedStackSample, stack: &[u8]) {
        assert_eq!((sample.pid, sample.tid), (42, 43));
        assert_eq!(sample.ip, STARNIX_PC);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                STARNIX_PC,
                perf_callchain_context_PERF_CONTEXT_USER,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        assert_eq!(sample.regs, thread_state_regs(STARNIX_PC, STARNIX_SP, 0)[..33].to_vec());
        assert_eq!(sample.stack, stack);
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_reports_guest_regs_and_stack() {
        let starnix_stack = stack_bytes(0x10, 64);
        let guest_x: [u64; 31] = std::array::from_fn(|i| 0xb000 + i as u64);
        let restricted_state = restricted_state_bytes(&guest_x, GUEST_SP, GUEST_PC, 0);
        let guest_stack = stack_bytes(0x80, 48);
        let blob = stack_sample_blob(
            &thread_state_regs(STARNIX_PC, STARNIX_SP, 0),
            &[
                (STARNIX_SP, &starnix_stack),
                (RESTRICTED_STATE_ADDR, &restricted_state),
                (GUEST_SP, &guest_stack),
            ],
        );

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!((sample.pid, sample.tid), (42, 43));
        // The Starnix PC is only reported as kernel execution; the user
        // section starts at the guest PC.
        assert_eq!(sample.ip, STARNIX_PC);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                STARNIX_PC,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        let mut expected_regs = guest_x.to_vec();
        expected_regs.extend([GUEST_SP, GUEST_PC]);
        assert_eq!(sample.regs, expected_regs);
        assert_eq!(sample.stack, guest_stack);
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_reports_aarch32_guest_regs() {
        const GUEST_PC32: u64 = 0x0040_1234;
        const GUEST_SP32: u64 = 0xff80_1000;
        let starnix_stack = stack_bytes(0x10, 64);
        let mut guest_x: [u64; 31] = std::array::from_fn(|i| 0xc000 + i as u64);
        guest_x[AARCH32_REG_SP] = GUEST_SP32;
        // AArch32 execution does not update the sp field; it holds a stale
        // value that no capture covers.
        let restricted_state = restricted_state_bytes(
            &guest_x,
            0xdead_0000,
            GUEST_PC32,
            zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32,
        );
        let guest_stack = stack_bytes(0x40, 32);
        let blob = stack_sample_blob(
            &thread_state_regs(STARNIX_PC, STARNIX_SP, 0),
            &[
                (STARNIX_SP, &starnix_stack),
                (RESTRICTED_STATE_ADDR, &restricted_state),
                (GUEST_SP32, &guest_stack),
            ],
        );

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32 as u64);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                STARNIX_PC,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC32,
            ]
        );
        assert_eq!(sample.regs.len(), 33);
        assert_eq!(sample.regs[AARCH32_REG_SP], GUEST_SP32);
        assert_eq!(sample.regs[AARCH32_REG_PC], GUEST_PC32);
        assert_eq!(sample.regs[AARCH64_REG_PC], GUEST_PC32);
        // The sp and lr slots carry R13 and R14.
        assert_eq!(sample.regs[AARCH64_REG_SP], GUEST_SP32);
        assert_eq!(sample.regs[AARCH64_REG_LR], guest_x[AARCH32_REG_LR]);
        assert_eq!(sample.regs[0], 0xc000);
        assert_eq!(sample.stack, guest_stack);
    }

    #[::fuchsia::test]
    async fn test_stack_sample_in_guest_reports_thread_regs() {
        // A thread suspended in restricted mode reports the guest registers
        // itself; the restricted state then holds stale values and is
        // ignored.
        let guest_stack = stack_bytes(0x20, 64);
        let stale_stack = stack_bytes(0x90, 64);
        const STALE_SP: u64 = GUEST_SP + 0x10000;
        #[cfg(target_arch = "aarch64")]
        let stale_state = restricted_state_bytes(&[0x5a; 31], STALE_SP, GUEST_PC + 0x100, 0);
        #[cfg(not(target_arch = "aarch64"))]
        let stale_state = vec![0x5au8; 280];
        let thread_regs = thread_state_regs(GUEST_PC, GUEST_SP + 16, 0);
        let blob = stack_sample_blob(
            &thread_regs,
            &[
                (GUEST_SP, &guest_stack),
                (RESTRICTED_STATE_ADDR, &stale_state),
                (STALE_SP, &stale_stack),
            ],
        );

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!(sample.ip, GUEST_PC);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        assert_eq!(sample.regs, thread_regs[..33].to_vec());
        // The stack starts exactly at the SP, inside the first capture.
        assert_eq!(sample.stack, guest_stack[16..].to_vec());
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_guest_aarch32_reports_sp_lr_from_r13_r14() {
        // A thread suspended in AArch32 restricted mode: Zircon reports its
        // stack pointer in r[13] and link register in r[14]; the sp and lr
        // slots hold SP_EL0 and X30, which AArch32 execution never updates.
        const GUEST_PC32: u64 = 0x0040_1234;
        const GUEST_SP32: u64 = 0xff80_1000;
        const GUEST_LR32: u64 = 0x0040_5678;
        const STALE_SP: u64 = 0xdead_0000;
        const STALE_LR: u64 = 0xbeef_0000;
        let guest_stack = stack_bytes(0x30, 64);
        let mut thread_regs =
            thread_state_regs(GUEST_PC32, STALE_SP, zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u64);
        thread_regs[AARCH32_REG_SP] = GUEST_SP32;
        thread_regs[AARCH32_REG_LR] = GUEST_LR32;
        thread_regs[AARCH64_REG_LR] = STALE_LR;
        let blob = stack_sample_blob(&thread_regs, &[(GUEST_SP32, &guest_stack)]);

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!(sample.ip, GUEST_PC32);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC32,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_32 as u64);
        assert_eq!(sample.regs.len(), 33);
        assert_eq!(sample.regs[AARCH64_REG_SP], GUEST_SP32);
        assert_eq!(sample.regs[AARCH64_REG_LR], GUEST_LR32);
        assert_eq!(sample.regs[AARCH32_REG_SP], GUEST_SP32);
        assert_eq!(sample.regs[AARCH32_REG_LR], GUEST_LR32);
        assert_eq!(sample.regs[AARCH32_REG_PC], GUEST_PC32);
        assert_eq!(sample.regs[AARCH64_REG_PC], GUEST_PC32);
        assert_eq!(sample.regs[0], 0xa000);
        // The stack is selected by R13, not by the stale sp slot.
        assert_eq!(sample.stack, guest_stack);
    }

    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_without_restricted_state_reports_thread_regs() {
        let starnix_stack = stack_bytes(0x10, 64);
        let thread_regs = thread_state_regs(STARNIX_PC, STARNIX_SP, 0);
        let blob = stack_sample_blob(&thread_regs, &[(STARNIX_SP, &starnix_stack)]);

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_reports_thread_state(&sample, &starnix_stack);
    }

    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_without_captures_is_reported() {
        let thread_regs = thread_state_regs(STARNIX_PC, STARNIX_SP, 0);
        let blob = stack_sample_blob(&thread_regs, &[]);

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_reports_thread_state(&sample, &[]);
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_ignores_unusable_restricted_state() {
        let starnix_stack = stack_bytes(0x10, 64);
        let guest_stack = stack_bytes(0x80, 48);
        let guest_x: [u64; 31] = std::array::from_fn(|i| 0xb000 + i as u64);
        let valid_state = restricted_state_bytes(&guest_x, GUEST_SP, GUEST_PC, 0);
        let cases: [(&str, u64, Vec<u8>); 6] = [
            (
                "pc above the guest address space",
                RESTRICTED_STATE_ADDR,
                restricted_state_bytes(&guest_x, GUEST_SP, STARNIX_PC, 0),
            ),
            (
                "pc below the guest address space",
                RESTRICTED_STATE_ADDR,
                restricted_state_bytes(&guest_x, GUEST_SP, 0x1000, 0),
            ),
            (
                "sp above the guest address space",
                RESTRICTED_STATE_ADDR,
                restricted_state_bytes(&guest_x, STARNIX_SP, GUEST_PC, 0),
            ),
            ("not yet written", RESTRICTED_STATE_ADDR, restricted_state_bytes(&[0; 31], 0, 0, 0)),
            ("not page-aligned", RESTRICTED_STATE_ADDR + 8, valid_state.clone()),
            ("short read", RESTRICTED_STATE_ADDR, valid_state[..valid_state.len() - 8].to_vec()),
        ];
        for (name, state_addr, state) in cases {
            let blob = stack_sample_blob(
                &thread_state_regs(STARNIX_PC, STARNIX_SP, 0),
                &[(STARNIX_SP, &starnix_stack), (state_addr, &state), (GUEST_SP, &guest_stack)],
            );

            let sample = process_stack_sample(blob, ALL_REGS_MASK);

            println!("case: {name}");
            assert_reports_thread_state(&sample, &starnix_stack);
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_without_guest_stack_reports_guest_regs() {
        // Even if the guest stack chunk is missing (e.g. the guest SP was
        // unmapped when sampled), guest registers are still reported with an
        // empty stack.
        let starnix_stack = stack_bytes(0x10, 64);
        let guest_x: [u64; 31] = std::array::from_fn(|i| 0xb000 + i as u64);
        let restricted_state = restricted_state_bytes(&guest_x, GUEST_SP, GUEST_PC, 0);
        let thread_regs = thread_state_regs(STARNIX_PC, STARNIX_SP, 0);
        let blob = stack_sample_blob(
            &thread_regs,
            &[(STARNIX_SP, &starnix_stack), (RESTRICTED_STATE_ADDR, &restricted_state)],
        );

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!((sample.pid, sample.tid), (42, 43));
        assert_eq!(sample.ip, STARNIX_PC);
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                STARNIX_PC,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        let mut expected_regs = guest_x.to_vec();
        expected_regs.extend([GUEST_SP, GUEST_PC]);
        assert_eq!(sample.regs, expected_regs);
        assert!(sample.stack.is_empty());
    }

    #[cfg(target_arch = "aarch64")]
    #[::fuchsia::test]
    async fn test_stack_sample_in_starnix_with_uncovered_guest_sp_reports_guest_regs() {
        // If no chunk covers the guest SP, report guest registers with an
        // empty stack rather than picking the Starnix stack.
        let starnix_stack = stack_bytes(0x10, 64);
        let other_stack = stack_bytes(0x80, 48);
        let guest_x: [u64; 31] = std::array::from_fn(|i| 0xb000 + i as u64);
        let restricted_state = restricted_state_bytes(&guest_x, GUEST_SP + 0x10000, GUEST_PC, 0);
        let blob = stack_sample_blob(
            &thread_state_regs(STARNIX_PC, STARNIX_SP, 0),
            &[
                (STARNIX_SP, &starnix_stack),
                (RESTRICTED_STATE_ADDR, &restricted_state),
                (GUEST_SP, &other_stack),
            ],
        );

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        assert_eq!(sample.regs[AARCH64_REG_SP], GUEST_SP + 0x10000);
        assert_eq!(sample.regs[AARCH64_REG_PC], GUEST_PC);
        assert!(sample.stack.is_empty());
        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                STARNIX_PC,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC,
            ]
        );
    }

    #[::fuchsia::test]
    async fn test_stack_sample_in_guest_without_stack_capture_is_reported() {
        let thread_regs = thread_state_regs(GUEST_PC, GUEST_SP, 0);
        let blob = stack_sample_blob(&thread_regs, &[]);

        let sample = process_stack_sample(blob, ALL_REGS_MASK);

        assert_eq!(
            sample.callchain,
            vec![
                perf_callchain_context_PERF_CONTEXT_KERNEL,
                perf_callchain_context_PERF_CONTEXT_USER,
                GUEST_PC,
            ]
        );
        assert_eq!(sample.abi, perf_sample_regs_abi_PERF_SAMPLE_REGS_ABI_64 as u64);
        assert_eq!(sample.regs, thread_regs[..33].to_vec());
        assert!(sample.stack.is_empty());
    }
}
