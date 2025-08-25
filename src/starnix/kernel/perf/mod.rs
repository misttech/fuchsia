// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fuchsia_component::client::connect_to_protocol;
use zerocopy::IntoBytes;
use {fidl_fuchsia_cpu_profiler as profiler, fuchsia_async};

use regex::Regex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::io::AsyncReadExt;
use starnix_logging::{log_info, log_warn, track_stub};
use starnix_sync::{FileOpsCore, Locked, RwLock, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::arch32::{
    PERF_EVENT_IOC_DISABLE, PERF_EVENT_IOC_ENABLE, PERF_EVENT_IOC_ID,
    PERF_EVENT_IOC_MODIFY_ATTRIBUTES, PERF_EVENT_IOC_PAUSE_OUTPUT, PERF_EVENT_IOC_PERIOD,
    PERF_EVENT_IOC_QUERY_BPF, PERF_EVENT_IOC_REFRESH, PERF_EVENT_IOC_RESET, PERF_EVENT_IOC_SET_BPF,
    PERF_EVENT_IOC_SET_FILTER, PERF_EVENT_IOC_SET_OUTPUT, PERF_RECORD_MISC_KERNEL,
    perf_event_sample_format_PERF_SAMPLE_CALLCHAIN, perf_event_sample_format_PERF_SAMPLE_IP,
    perf_event_sample_format_PERF_SAMPLE_TID, perf_event_type_PERF_RECORD_SAMPLE,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::UserRef;
use starnix_uapi::{
    error, perf_event_attr, perf_event_header, perf_event_read_format_PERF_FORMAT_GROUP,
    perf_event_read_format_PERF_FORMAT_ID, perf_event_read_format_PERF_FORMAT_LOST,
    perf_event_read_format_PERF_FORMAT_TOTAL_TIME_ENABLED,
    perf_event_read_format_PERF_FORMAT_TOTAL_TIME_RUNNING, tid_t, uapi,
};
use zx::AsHandleRef;
use zx::sys::zx_system_get_page_size;

static READ_FORMAT_ID_GENERATOR: AtomicU64 = AtomicU64::new(0);
// Default sample period of one sample per millisecond.
static DEFAULT_SAMPLE_PERIOD: u64 = 1000000;
// Default buffer size to read from socket (for sampling data).
static DEFAULT_CHUNK_SIZE: usize = 4096;
static ESTIMATED_MMAP_BUFFER_SIZE: u64 = 16384; // 4096 * 4, page size * 4.

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
    _rf_lost: u64,
    disabled: u64,
    sample_type: u64,
    // Handle to blob that stores all the perf data that a user may want.
    // At the moment it only stores some metadata and backtraces (bts).
    perf_data_vmo: zx::Vmo,
    // Remember to increment this offset as the number of pages increases.
    // Currently we just have a bound of 1 page_size of information.
    vmo_write_offset: u64,
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
        vmo_write_offset: u64,
    ) -> PerfEventFileState {
        PerfEventFileState {
            attr,
            rf_value,
            most_recent_enabled_time: 0,
            total_time_running: 0,
            rf_id: 0,
            _rf_lost: 0,
            disabled,
            sample_type,
            perf_data_vmo,
            vmo_write_offset,
        }
    }
}

struct PerfEventFile {
    _tid: tid_t,
    _cpu: i32,
    perf_event_file: RwLock<PerfEventFileState>,
}

// PerfEventFile object that implements FileOps.
// See https://man7.org/linux/man-pages/man2/perf_event_open.2.html for
// implementation details.
// This object can be saved as a FileDescriptor.
impl FileOps for PerfEventFile {
    // Don't need to implement seek or sync for PerfEventFile.
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    // See "Reading results" section of https://man7.org/linux/man-pages/man2/perf_event_open.2.html.
    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        // Create/calculate and return the ReadFormatData object.
        // If we create it earlier we might want to change it and it's immutable once created.
        let read_format_data = {
            // Once we get the `value` or count from kernel, we can change this to a read()
            // call instead of write().
            let mut perf_event_file = self.perf_event_file.write();
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
        _locked: &mut Locked<Unlocked>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        op: u32,
        _arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        track_stub!(
            TODO("https://fxbug.dev/405463320"),
            "[perf_event_open] implement PERF_IOC_FLAG_GROUP"
        );
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

                // SAFETY: sample_period is a u64 field in a union with u64 sample_freq.
                // This is always sound regardless of the union's tag.
                if perf_event_file.attr.freq() == 0
                    && unsafe { perf_event_file.attr.__bindgen_anon_1.sample_period != 0 }
                {
                    let vmo_handle_copy = perf_event_file
                        .perf_data_vmo
                        .as_handle_ref()
                        .duplicate(zx::Rights::SAME_RIGHTS);

                    track_stub!(
                        TODO("https://fxbug.dev/438271095"),
                        "[perf_event_open] swap to spawn_future()"
                    );
                    let mut executor = fuchsia_async::LocalExecutor::new();
                    executor.run_singlethreaded(async {
                        match set_up_profiler().await {
                            Ok((session_proxy, client)) => {
                                track_stub!(
                                    TODO("https://fxbug.dev/422502681"),
                                    "[perf_event_open] don't hardcode profiling duration"
                                );
                                let _ = collect_sample(
                                    session_proxy,
                                    client,
                                    Duration::from_millis(100),
                                    &zx::Vmo::from(vmo_handle_copy.unwrap()),
                                    perf_event_file.sample_type,
                                    perf_event_file.vmo_write_offset,
                                )
                                .await;
                            }
                            Err(e) => log_warn!("Failed to profile: {}", e),
                        };
                    });
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

    // Gets called when mmap() is called.
    // Immediately before sampling, this should get called by the user (e.g. the test
    // or Perfetto). We will then write the metadata to the VMO and return the pointer to it.
    fn get_memory(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        length: Option<usize>,
        _prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        let buffer_size: u64 = length.unwrap_or(0) as u64;
        if buffer_size == 0 {
            return error!(EINVAL);
        }
        // Create metadata page. Currently we hardcode everything just to get something E2E working.
        let mut metadata = Vec::<u8>::new();
        // version
        metadata.extend(1_u32.to_ne_bytes());
        // compat version
        metadata.extend(2_u32.to_ne_bytes());
        // lock
        metadata.extend(2_u32.to_ne_bytes());
        // index
        metadata.extend(2_u32.to_ne_bytes());
        // offset
        metadata.extend(19337_i64.to_ne_bytes());
        // time_enabled
        metadata.extend(0_u64.to_ne_bytes());
        // time_running
        metadata.extend(0_u64.to_ne_bytes());
        // capabilities
        metadata.extend(30_u64.to_ne_bytes());
        // All the fields between pmc_width and reserved (inclusive).
        metadata.extend(vec![0; 976].as_slice());
        // data_head (see below comment re: PERF_RECORD_SAMPLE).
        metadata.extend(32_u64.to_ne_bytes());
        // data_tail
        metadata.extend(0_u64.to_ne_bytes());
        // data_offset. Don't mind the unsafe block.
        // https://fuchsia.dev/reference/syscalls/system_get_page_size#errors
        // says it cannot fail, but rust compiler needs it.
        let page_size: u64 = unsafe { zx_system_get_page_size() } as u64;
        metadata.extend(page_size.to_ne_bytes());
        // data_size
        metadata.extend(((buffer_size - page_size) as u64).to_ne_bytes());
        // The remaining metadata are not defined for now.

        // Write metadata to VMO and return. Later during IOC_ENABLE, samples will be
        // appended.
        let perf_event_file = self.perf_event_file.read();
        match perf_event_file
            .perf_data_vmo
            .write(&metadata, 0 /* This is the offset, not the length to write */)
        {
            Ok(()) => {
                // VMO does not implement Copy trait. We duplicate the VMO handle
                // so that we can pass it to the MemoryObject.
                let vmo_handle_copy = match perf_event_file
                    .perf_data_vmo
                    .as_handle_ref()
                    .duplicate(zx::Rights::SAME_RIGHTS)
                {
                    Ok(h) => h,
                    Err(_) => return error!(EINVAL),
                };

                let memory = MemoryObject::Vmo(vmo_handle_copy.into());
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
        _locked: &mut Locked<FileOpsCore>,
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
//    0x0000 0x0009
//    0x0001
//    0x0040
//    0x0000 0x0000 0x0000 0x006F
//    0x0000 0x0000 0x0000 0x0005
//    0x0000 0x0000 0x0000 0x0010
//    0x0000 0x0000 0x0000 0x0003
//    0x0000 0x0000 0x0000 0x006F
//    0x0000 0x0000 0x0000 0x00DE
//    0x0000 0x0000 0x0000 0x014D
fn write_record_to_vmo(
    perf_record_sample: PerfRecordSample,
    perf_data_vmo: &zx::Vmo,
    sample_type: u64,
    offset: u64,
) -> () {
    track_stub!(
        TODO("https://fxbug.dev/432501467"),
        "[perf_event_open] determines whether the record is KERNEL or USER"
    );
    track_stub!(
        TODO("https://fxbug.dev/433748755"),
        "[perf_event_open] figure out why this can't be lower than 40"
    );
    let perf_event_header = perf_event_header {
        type_: perf_event_type_PERF_RECORD_SAMPLE,
        misc: PERF_RECORD_MISC_KERNEL as u16,
        // perf_event_header size - size of the record including header.
        // For the current example:
        // header: 32 + 16 + 16
        // PERF_RECORD_SAMPLE: 64 (sample_id) + 64 (ip) + 32 (pid) + 32 (tid)
        // total: 256 bits = 32 bytes
        size: 32,
    };

    match zx::Vmo::write(&perf_data_vmo, &perf_event_header.as_bytes(), offset) {
        Ok(_) => (),
        Err(e) => log_warn!("Failed to write perf_event_header: {}", e),
    }

    let mut sample = Vec::<u8>::new();
    // ip
    if (sample_type & perf_event_sample_format_PERF_SAMPLE_IP as u64) != 0 {
        sample.extend(perf_record_sample.ips[0].to_ne_bytes());
    }

    if (sample_type & perf_event_sample_format_PERF_SAMPLE_TID as u64) != 0 {
        // pid
        sample.extend(perf_record_sample.pid.expect("missing pid").to_ne_bytes());
        // tid
        sample.extend(perf_record_sample.tid.expect("missing tid").to_ne_bytes());
    }

    if (sample_type & perf_event_sample_format_PERF_SAMPLE_CALLCHAIN as u64) != 0 {
        // nr
        sample.extend(perf_record_sample.ips.len().to_ne_bytes());

        // ips[nr] - list of ips, u64 per ip.
        for i in perf_record_sample.ips {
            sample.extend(i.to_ne_bytes());
        }
    }
    // The remaining data are not defined for now.

    match zx::Vmo::write(
        &perf_data_vmo,
        &sample,
        offset + (std::mem::size_of::<perf_event_header>() as u64),
    ) {
        Ok(_) => return (),
        Err(e) => log_warn!("Failed to write PerfRecordSample to VMO due to: {}", e),
    }
}

#[derive(Debug, Clone)]
struct PerfRecordSample {
    pid: Option<u32>,
    tid: Option<u32>,
    // Instruction pointers (currently this is the address). First one is `ip` param.
    ips: Vec<u64>,
}

// Parses a backtrace (bt) to obtain the params for a PerfRecordSample. Example:
//
// 1234                     pid
// 5555                     tid
// {{{bt:0:0x1111:pc}}}    {{{bt:frame_number:address:type}}}
// {{{bt:1:0x2222:ra}}}
// {{{bt:2:0x3333:ra}}}
//
// Results in:
// PerfRecordSample { pid: 1234, tid: 5555, nr: 3, ips: [0x1111, 0x2222, 0x3333] }
fn parse_perf_record_sample_format(backtrace: &str) -> Option<PerfRecordSample> {
    let mut pid: Option<u32> = None;
    let mut tid: Option<u32> = None;
    let mut ips: Vec<u64> = Vec::new();
    let mut numbers_found = 0;
    track_stub!(TODO("https://fxbug.dev/437171287"), "[perf_event_open] handle regex nuances");
    let backtrace_regex =
        Regex::new(r"^\s*\{\{\{bt:\d+:((0x[0-9a-fA-F]+)):(?:pc|ra)\}\}\}\s*$").unwrap();

    for line in backtrace.lines() {
        let trimmed_line = line.trim();
        // Try to parse as a raw number (for PID/TID).
        if numbers_found < 2 {
            if let Ok(num) = trimmed_line.parse::<u32>() {
                if numbers_found == 0 {
                    pid = Some(num);
                } else {
                    tid = Some(num);
                }
                numbers_found += 1;
                continue;
            }
        }

        // Try to parse as a backtrace line.
        if let Some(parsed_bt) = backtrace_regex.captures(trimmed_line) {
            let address_str = parsed_bt.get(1).unwrap().as_str();
            if let Ok(ip_addr) = u64::from_str_radix(address_str.trim_start_matches("0x"), 16) {
                ips.push(ip_addr);
            }
        }
    }

    if pid == None || tid == None || ips.is_empty() {
        // This data chunk might've been an {{{mmap}}} chunk, and not a {{{bt}}}.
        log_info!("No ips while getting PerfRecordSample");
        None
    } else {
        Some(PerfRecordSample { pid: pid, tid: tid, ips: ips })
    }
}

async fn set_up_profiler() -> Result<(profiler::SessionProxy, fidl::AsyncSocket), Errno> {
    // Configuration for how we want to sample.
    let sample = profiler::Sample {
        callgraph: Some(profiler::CallgraphConfig {
            strategy: Some(profiler::CallgraphStrategy::FramePointer),
            ..Default::default()
        }),
        ..Default::default()
    };

    let sampling_config = profiler::SamplingConfig {
        period: Some(DEFAULT_SAMPLE_PERIOD),
        timebase: Some(profiler::Counter::PlatformIndependent(profiler::CounterId::Nanoseconds)),
        sample: Some(sample),
        ..Default::default()
    };

    let tasks = vec![
        // Should return around 1500 ips for 100 millis.
        profiler::Task::SystemWide(profiler::SystemWide {}),
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

// Collects samples and puts backtrace in VMO.
// - Starts and stops sampling for a duration.
// - Reads in the buffer from the socket for that duration in chunks.
// - Parses the buffer backtraces into PERF_RECORD_SAMPLE format.
// - Writes the PERF_RECORD_SAMPLE into VMO.
async fn collect_sample(
    session_proxy: profiler::SessionProxy,
    mut client: fidl::AsyncSocket,
    duration: Duration,
    perf_data_vmo: &zx::Vmo,
    sample_type: u64,
    offset: u64,
) -> Result<(), Errno> {
    let start_request = profiler::SessionStartRequest {
        buffer_results: Some(true),
        buffer_size_mb: Some(8 as u64),
        ..Default::default()
    };
    let _ = session_proxy.start(&start_request).await.expect("Failed to start profiling");

    // Hardcode a duration so that samples can be collected. This is currently solely used to
    // demonstrate that an E2E implementation of sample collection works.
    track_stub!(
        TODO("https://fxbug.dev/428974888"),
        "[perf_event_open] don't hardcode sleep; test/user should decide sample duration"
    );
    let _ = fuchsia_async::Timer::new(duration).await;

    let stats = session_proxy.stop().await;
    let samples_collected = match stats {
        Ok(stats) => stats.samples_collected.unwrap(),
        Err(e) => return error!(EINVAL, e),
    };

    track_stub!(
        TODO("https://fxbug.dev/422502681"),
        "[perf_event_open] symbolize sample output and delete the println"
    );
    log_info!("profiler samples_collected: {:?}", samples_collected);

    // Read chunks of sampling data from socket in this buffer temporarily. We will parse
    // the data and write it into the output VMO (the one mmap points to).
    let mut buffer = vec![0; DEFAULT_CHUNK_SIZE];
    loop {
        // Attempt to read data. This awaits until data is available, EOF, or error.
        let socket_data = client.read(&mut buffer).await;

        match socket_data {
            Ok(0) => {
                // Peer closed the socket. This is the normal end of the stream.
                log_info!("[perf_event_open] Finished reading from socket.");
                break;
            }
            Ok(bytes_read) => {
                // Receive data in format {{{...}}}.
                let received_data = match std::str::from_utf8(&buffer[..bytes_read]) {
                    Ok(data) => data,
                    Err(e) => return error!(EINVAL, e),
                };
                // Parse data to PerfRecordSample struct.
                if let Some(perf_record_sample) = parse_perf_record_sample_format(received_data) {
                    write_record_to_vmo(perf_record_sample, perf_data_vmo, sample_type, offset);
                }
            }
            Err(e) => {
                log_warn!("[perf_event_open] Error reading from socket: {:?}", e);
                break;
            }
        }
    }

    let reset_status = session_proxy.reset().await;
    return match reset_status {
        Ok(_) => Ok(()),
        Err(e) => error!(EINVAL, e),
    };
}

pub fn sys_perf_event_open(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    attr: UserRef<perf_event_attr>,
    // Note that this is pid in Linux docs.
    tid: tid_t,
    cpu: i32,
    group_fd: FdNumber,
    _flags: u64,
) -> Result<SyscallResult, Errno> {
    if tid == -1 && cpu == -1 {
        return error!(EINVAL);
    }
    if group_fd != FdNumber::from_raw(-1) {
        track_stub!(TODO("https://fxbug.dev/409619971"), "[perf_event_open] implement group_fd");
        return error!(ENOSYS);
    }
    if tid > 0 {
        track_stub!(TODO("https://fxbug.dev/409621963"), "[perf_event_open] implement tid > 0");
        return error!(ENOSYS);
    }

    // So far, the implementation only sets the read_data_format according to the "Reading results"
    // section of https://man7.org/linux/man-pages/man2/perf_event_open.2.html for a single event.
    // Other features will be added in the future (see below track_stubs).
    let perf_event_attrs: perf_event_attr = current_task.read_object(attr)?;

    // https://fuchsia.dev/reference/syscalls/system_get_page_size#errors
    // says it cannot fail, but rust compiler needs it.
    let page_size: u64 = unsafe { zx_system_get_page_size() } as u64;
    let mut perf_event_file = PerfEventFileState::new(
        perf_event_attrs,
        0,
        perf_event_attrs.disabled(),
        perf_event_attrs.sample_type,
        zx::Vmo::create(ESTIMATED_MMAP_BUFFER_SIZE).unwrap(),
        page_size, // Start with this amount, we can increment as we write.
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
    if (read_format & perf_event_read_format_PERF_FORMAT_ID as u64) != 0 {
        // Adds a 64-bit unique value that corresponds to the event group.
        perf_event_file.rf_id = READ_FORMAT_ID_GENERATOR.fetch_add(1, Ordering::Relaxed);
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

    let file = Box::new(PerfEventFile {
        _tid: tid,
        _cpu: cpu,
        perf_event_file: RwLock::new(perf_event_file),
    });
    // TODO: https://fxbug.dev/404739824 - Confirm whether to handle this as a "private" node.
    let file_handle =
        Anon::new_private_file(locked, current_task, file, OpenFlags::RDWR, "[perf_event]");
    let file_descriptor: Result<FdNumber, Errno> =
        current_task.add_file(locked, file_handle, FdFlags::empty());

    match file_descriptor {
        Ok(fd) => Ok(fd.into()),
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
#[cfg(feature = "arch32")]
mod arch32 {
    pub use super::sys_perf_event_open as sys_arch32_perf_event_open;
}

#[cfg(feature = "arch32")]
pub use arch32::*;

use crate::mm::memory::MemoryObject;
use crate::mm::{MemoryAccessorExt, ProtectionFlags};
use crate::task::CurrentTask;
use crate::vfs::{Anon, FdFlags, FdNumber, FileObject, FileOps, InputBuffer, OutputBuffer};
use crate::{fileops_impl_nonseekable, fileops_impl_noop_sync};
