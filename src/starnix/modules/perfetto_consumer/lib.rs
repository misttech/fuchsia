// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use crate::blob_sink::{BlobSink, TraceBufferSink, WriteAttempt};
use anyhow::bail;
use fuchsia_trace::{BufferingMode, ProlongedContext, TraceState, category_enabled, trace_state};
use fuchsia_trace_observer::TraceObserver;
use futures::{SinkExt, StreamExt};
use perfetto_protos::perfetto::protos::trace_config::buffer_config::FillPolicy;
use perfetto_protos::perfetto::protos::trace_config::{BufferConfig, DataSource};
use perfetto_protos::perfetto::protos::{
    DataSourceConfig, DisableTracingRequest, EnableTracingRequest, FreeBuffersRequest,
    FtraceConfig, ReadBuffersRequest, TraceConfig, ipc_frame,
};
use perfetto_trace_protos::perfetto::protos::frame_timeline_event::{
    ActualDisplayFrameStart, ActualSurfaceFrameStart, Event, ExpectedDisplayFrameStart,
    ExpectedSurfaceFrameStart,
};
use perfetto_trace_protos::perfetto::protos::ftrace_event::Event::Print;
use perfetto_trace_protos::perfetto::protos::trace_packet;
use starnix_core::security;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::tracing::PidKoidSession;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsString;
use starnix_logging::{CATEGORY_ATRACE, log_debug, log_error, log_info, log_warn};
use starnix_perfetto_trace_decoder::{decode_read_buffers_response, decode_trace, encode_trace};

use starnix_uapi::errors::Errno;

mod atrace;
mod blob_sink;
#[cfg(test)]
mod testing;

const PERFETTO_BUFFER_SIZE_KB: u32 = 63488;

/// Maximum number of Perfetto bytes carried by one FXT blob record.
///
/// The drain writes each Perfetto packet, framed as a field of a `Trace` message, as one
/// record, and splits only framed packets longer than this limit. Packets can be that long:
/// upstream traced_probes can put the ftrace events from several ring-buffer pages into one
/// `FtraceEventBundle` packet.
///
/// The value balances these limits:
/// - An FXT blob record holds at most `fxt::MAX_RECORD_SIZE_WORDS` words: the header word,
///   the inline blob name and the payload, each padded to 8 bytes (see
///   [`fxt::blob::build_blob_record()`]).
/// - Bigger blobs split a long packet into fewer records, so there is less header and name
///   overhead per byte.
/// - A record that does not fit in the rest of the current rolling buffer goes into the next
///   one, and the rest of the current one stays unused, so each buffer switch can waste up to
///   one record. With trace_manager's minimum default trace buffer, 1 MiB, each rolling
///   buffer is about 480 KiB, so a 4 KiB blob wastes under 1% of one. A per-provider buffer
///   size is also a whole number of MiB, so every provider buffer that can hold records is
///   at least that large.
const MAX_BLOB_PAYLOAD_SIZE: usize = 4096;

/// Writes `data` to `sink` as Perfetto blob records, split into chunks of at most
/// `MAX_BLOB_PAYLOAD_SIZE` bytes. Each chunk is written by [`forward_blob()`] with
/// `retry_on_full`.
///
/// Returns the number of payload bytes written (`data.len()`), or `None` if a chunk was
/// dropped. Chunks written before the dropped one stay in the trace buffer, and later chunks
/// are not attempted.
fn forward_packet(sink: &mut impl BlobSink, retry_on_full: bool, data: &[u8]) -> Option<usize> {
    let mut bytes_written = 0;
    for chunk in data.chunks(MAX_BLOB_PAYLOAD_SIZE) {
        forward_blob(sink, retry_on_full, chunk)?;
        bytes_written += chunk.len();
    }
    Some(bytes_written)
}

/// Writes `blob_data` to `sink` as one Perfetto blob record. With `retry_on_full`, waits
/// 100 ms and tries again for as long as the trace buffer is full.
///
/// Returns `None` if the record was dropped because the buffer is full and `retry_on_full` is
/// false, no trace context is available, or `blob_data` does not fit in one record.
fn forward_blob(sink: &mut impl BlobSink, retry_on_full: bool, blob_data: &[u8]) -> Option<()> {
    loop {
        match sink.try_write_blob(blob_data) {
            WriteAttempt::Wrote => return Some(()),
            WriteAttempt::TracingStopped => return None,
            WriteAttempt::TooLong => return None,
            // Only streaming mode retries. In oneshot mode a full buffer stays full. In circular
            // mode trace-engine frees the other rolling buffer as soon as the last writer
            // releases its context, so a full buffer is rare and usually short-lived. Losing
            // the record is acceptable there, so drop it rather than wait.
            WriteAttempt::BufferFull if !retry_on_full => return None,
            WriteAttempt::BufferFull => {
                // We're writing records pretty quick here, we're just forwarding data from
                // perfetto with no breaks. trace_manager might not be able to keep up if it's
                // also servicing other trace-providers. We want to back off if we find that we
                // run out of space.
                //
                // TODO(b/304532640)
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// State needed to act upon trace state changes.
struct CallbackState {
    /// The previously observed trace state.
    prev_state: TraceState,
    /// Path to the Perfetto consumer socket.
    socket_path: FsString,
    /// Connection to the consumer socket, if it has been initialized. This gets initialized the
    /// first time it is needed.
    connection: Option<perfetto::Consumer>,
    /// Prolonged trace context to prevent the Fuchsia trace session from terminating while reading
    /// data from Perfetto. `TraceState::Started` stores it only once it has sent EnableTracing,
    /// and only `TraceState::Stopping` takes it, to disable Perfetto tracing and read the data.
    /// So it is `Some` exactly when Started has requested Perfetto tracing and Stopping hasn't run
    /// since. Holding it keeps trace-engine from reaching Stopped, so the session it belongs to
    /// can't end, and no other session can start, until Stopping releases it.
    prolonged_context: Option<ProlongedContext>,
    /// Partial trace packet returned from Perfetto but not yet written to Fuchsia.
    packet_data: Vec<u8>,

    /// Recording session for pid/koid mappings, held while tracing is started. Dropping
    /// it (on stop) releases this client's interest in the shared manager.
    session: Option<PidKoidSession>,
}

impl CallbackState {
    /// Returns the state for a consumer that hasn't seen any trace state change yet.
    fn new(socket_path: FsString) -> Self {
        Self {
            prev_state: TraceState::Stopped,
            socket_path,
            connection: None,
            prolonged_context: None,
            packet_data: Vec::new(),
            session: None,
        }
    }

    fn connection(
        &mut self,
        current_task: &CurrentTask,
    ) -> Result<&mut perfetto::Consumer, anyhow::Error> {
        match self.connection {
            None => {
                self.connection =
                    Some(perfetto::Consumer::new(current_task, self.socket_path.as_ref())?);
                Ok(self.connection.as_mut().unwrap())
            }
            Some(ref mut conn) => Ok(conn),
        }
    }

    /// Clears the state kept for a trace session that has ended. Leaves `prolonged_context`
    /// alone: only `TraceState::Stopping` releases it (see `prolonged_context`).
    fn handle_stopped(&mut self) {
        self.packet_data.clear();
        self.session = None;
    }

    fn on_state_change(
        &mut self,
        new_state: TraceState,
        current_task: &CurrentTask,
    ) -> Result<(), anyhow::Error> {
        let prev_state = self.prev_state;
        self.prev_state = new_state;
        log_debug!(
            "Perfetto consumer state change. new_state: {new_state:?}, prev_state: {prev_state:?}"
        );
        match new_state {
            TraceState::Started => {
                if self.prolonged_context.is_some() {
                    // Perfetto tracing has already been requested for the session whose context
                    // we hold, and that session hasn't ended (see `prolonged_context`). So this
                    // Started is a repeat for that session (see
                    // `start_perfetto_consumer_thread()` for how that happens), or a stale one
                    // for a session that ended before we acquired the context. Either way there
                    // is nothing to do.
                    return Ok(());
                }
                if prev_state == TraceState::Stopping {
                    // The observer reads the current state when it is woken, so it misses the
                    // previous session's Stopped if the next session has already started by then.
                    // A stale Stopping that disabled Perfetto tracing early (see Stopping below)
                    // can also be followed by Started. Either way the pid/koid session and packet
                    // data are left over from before.
                    log_warn!(
                        "Started received in {prev_state:?} state! Cleaning up then starting."
                    );
                    self.handle_stopped();
                }
                // After Stopped, or after a Started that didn't enable Perfetto tracing (see
                // below), there is nothing to clean up.

                // `ProlongedContext::acquire()` fails only while trace-engine's context count is
                // zero. With Started reported, that means one of:
                // - The session has already ended, for example because its Stopping and Stopped
                //   states were queued behind this one.
                // - The session is still starting. `trace_engine_start()` stores Started before
                //   it makes the count nonzero, and notifies observers only after that, so a
                //   `trace_state()` read in between returns Started while the count is zero.
                // Either way another state change follows: the end of the session, or its start
                // notification (see `start_perfetto_consumer_thread()` for why neither is
                // missed). Skip Perfetto until then. `prolonged_context` stays `None`, so a
                // later Stopping skips Perfetto and a later Started tries again.
                let Some(prolonged_context) = ProlongedContext::acquire() else {
                    log_info!("No trace context for Started; not enabling Perfetto tracing yet");
                    return Ok(());
                };
                let connection = self.connection(current_task)?;
                // A fixed set of data sources that may be of interest. As demand for other sources
                // is found, add them here, and it may become worthwhile to allow this set to be
                // configurable per trace session.
                let mut data_sources = vec![
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("track_event".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("android.surfaceflinger.frame".to_string()),
                            target_buffer: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("android.surfaceflinger.frametimeline".to_string()),
                            target_buffer: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ];
                if category_enabled(CATEGORY_ATRACE) {
                    data_sources.push(DataSource {
                        config: Some(DataSourceConfig {
                            name: Some("linux.ftrace".to_string()),
                            ftrace_config: Some(FtraceConfig {
                                ftrace_events: vec!["ftrace/print".to_string()],
                                // Enable all supported atrace categories. This could be improved
                                // in the future to be a subset that is configurable by each trace
                                // session.
                                atrace_categories: vec![
                                    "am".to_string(),
                                    "adb".to_string(),
                                    "aidl".to_string(),
                                    "dalvik".to_string(),
                                    "audio".to_string(),
                                    "binder_lock".to_string(),
                                    "binder_driver".to_string(),
                                    "bionic".to_string(),
                                    "camera".to_string(),
                                    "database".to_string(),
                                    "gfx".to_string(),
                                    "hal".to_string(),
                                    "input".to_string(),
                                    "network".to_string(),
                                    "nnapi".to_string(),
                                    "pm".to_string(),
                                    "power".to_string(),
                                    "rs".to_string(),
                                    "res".to_string(),
                                    "rro".to_string(),
                                    "sched".to_string(),
                                    "sm".to_string(),
                                    "ss".to_string(),
                                    "vibrator".to_string(),
                                    "video".to_string(),
                                    "view".to_string(),
                                    "webview".to_string(),
                                    "wm".to_string(),
                                ],
                                atrace_apps: vec!["*".to_string()],
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
                connection.enable_tracing(
                    current_task,
                    EnableTracingRequest {
                        trace_config: Some(TraceConfig {
                            buffers: vec![BufferConfig {
                                size_kb: Some(PERFETTO_BUFFER_SIZE_KB),
                                fill_policy: Some(FillPolicy::Discard.into()),
                                ..Default::default()
                            }],
                            data_sources,
                            ..Default::default()
                        }),
                        attach_notification_only: None,
                    },
                )?;
                // EnableTracing has been sent now, so hold the context until Stopping tears
                // Perfetto tracing down. If an error above returned early, EnableTracing wasn't
                // sent and the context was dropped without being stored.
                self.prolonged_context = Some(prolonged_context);
                // Once tracing has started, open a recording session so pid/koid
                // mappings are tracked for the duration of the trace.
                self.session = Some(current_task.kernel().trace_event_manager.open());
            }
            TraceState::Stopping => {
                // Hold the prolonged context so that trace-engine can't finish stopping while
                // we forward Perfetto data. Taking it out of `self` also drops it if we bail
                // below.
                let Some(_prolonged_context) = self.prolonged_context.take() else {
                    // Without a prolonged context Perfetto tracing isn't enabled (see
                    // `prolonged_context`), so there is no Perfetto session to disable or read.
                    if prev_state != TraceState::Started {
                        // If we receive a stop request and we don't think we're actually
                        // tracing, our local state likely desynced from the global trace state.
                        // Clean up our state and ensure we're stopped so we re-synchronize.
                        log_error!("Stopping received in {prev_state:?} state! Cleaning up.");
                        self.handle_stopped();
                    }
                    return Ok(());
                };
                // With the context held, this Stopping is for the session it belongs to, or a
                // stale one for a session that ended before we acquired the context. In the
                // stale case Perfetto tracing pauses for the current session until its own
                // Started, which is queued after this Stopping, enables it again, unless that
                // session is already stopping by then. Either way it is not left enabled.
                let start_time = std::time::Instant::now();

                let connection = self.connection(current_task)?;
                let disable_request =
                    connection.disable_tracing(current_task, DisableTracingRequest {})?;
                loop {
                    let frame = connection.next_frame_blocking(current_task)?;
                    if frame.request_id == Some(disable_request) {
                        break;
                    } else {
                        log_error!(
                            "Ignoring frame while looking for DisableTracingRequest: {frame:?}"
                        );
                    }
                }

                let read_buffers_request =
                    connection.read_buffers(current_task, ReadBuffersRequest {})?;

                // The buffering mode is fixed for the whole trace session.
                let retry_on_full = {
                    // A held prolonged context keeps trace-engine's context count above zero,
                    // so this can't fail.
                    let Some(context) = fuchsia_trace::Context::acquire() else {
                        bail!("Tracing stopped despite holding prolonged context");
                    };
                    context.buffering_mode() == BufferingMode::Streaming
                };
                let mut sink = TraceBufferSink;

                // IPC responses may be spread across multiple frames, so loop until we get a
                // message that indicates it is the last one. Additionally, if there are
                // unrelated messages on the socket (e.g. leftover from a previous trace
                // session), the loop will read past and ignore them.
                loop {
                    let frame = self.connection(current_task)?.next_frame_blocking(current_task)?;
                    if frame.request_id != Some(read_buffers_request) {
                        continue;
                    } else {
                        log_debug!(
                            "perfetto_consumer ignoring frame while looking for ReadBuffersRequest {read_buffers_request}: {frame:?}"
                        );
                    }
                    if let Some(ipc_frame::Msg::MsgInvokeMethodReply(reply)) = &frame.msg {
                        if let Ok(response) = decode_read_buffers_response(
                            reply.reply_proto.as_deref().unwrap_or(&[]),
                        ) {
                            for slice in &response.slices {
                                if let Some(data) = &slice.data {
                                    self.packet_data.extend(data);
                                }
                                if slice.last_slice_for_packet.unwrap_or(false) {
                                    let mut blob_data = Vec::new();
                                    // Packet field number = 1, length delimited type = 2.
                                    blob_data.push(1 << 3 | 2);
                                    // Push a varint encoded length.
                                    // See https://protobuf.dev/programming-guides/encoding/
                                    const HIGH_BIT: u8 = 0x80;
                                    const LOW_SEVEN_BITS: usize = 0x7F;
                                    let mut value = self.packet_data.len();
                                    while value >= HIGH_BIT as usize {
                                        blob_data.push((value & LOW_SEVEN_BITS) as u8 | HIGH_BIT);
                                        value >>= 7;
                                    }
                                    blob_data.push(value as u8);
                                    // `append` moves all data out of the passed Vec, so
                                    // s.packet_data will be empty after this call.
                                    blob_data.append(&mut self.packet_data);

                                    // At this point blob_data is a full Perfetto Trace protobuf.
                                    // Parse the data and replace the linux pids with their
                                    // corresponding koid.
                                    let rewritten =
                                        self.rewrite_pids(&blob_data).unwrap_or(blob_data);

                                    // Ignore a failure to write the packet here. We don't
                                    // return immediately because we want to allow the
                                    // remaining records to be recorded as dropped.
                                    //
                                    // Once we fill a buffer in oneshot mode, we expect to drop
                                    // the remaining packets here.
                                    //
                                    // Rather than logging here, allow the trace system to
                                    // aggregate the number of records dropped and we can query
                                    // the trace system later to determine if we dropped
                                    // records when it's more efficient to do so.
                                    let _ = forward_packet(&mut sink, retry_on_full, &rewritten);
                                }
                            }
                        } else {
                            log_error!("perfetto_consumer cannot decode protobuf from {reply:?}");
                        }
                        if reply.has_more != Some(true) {
                            break;
                        }
                    } else {
                        log_error!(
                            "perfetto_consumer ignoring non-MsgInvokeMethodReply message: {frame:?}"
                        );
                    }
                }
                // The response to a free buffers request does not have anything meaningful,
                // so we don't need to worry about tracking the request id to match to the
                // response.
                let _free_buffers_request_id = self
                    .connection(current_task)?
                    .free_buffers(current_task, FreeBuffersRequest { buffer_ids: vec![0] })?;
                let elapsed = start_time.elapsed().as_millis();
                log_info!(
                    "Perfetto frames copied, dropping prolonged trace context. Processing took {elapsed} ms"
                );
            }
            TraceState::Stopped => {
                if self.prolonged_context.is_some() {
                    // The session whose context we hold can't reach Stopped until Stopping
                    // releases the context, so this Stopped is stale: it is for a session that
                    // ended before we acquired the context. The held session's Stopping is still
                    // to come, so keep its context and pid/koid session until then.
                    return Ok(());
                }
                self.handle_stopped();
            }
        }
        Ok(())
    }

    fn rewrite_pids(&mut self, protobuf_blob: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut proto = decode_trace(protobuf_blob)?;
        for p in &mut proto.packet {
            if let Some(ref mut data) = p.data {
                match data {
                    trace_packet::Data::FrameTimelineEvent(frame_timeline_event) => {
                        if let Some(evt) = &mut frame_timeline_event.event {
                            // Update the linux pid to the Fuchsia pid. Each event has its own
                            // match arm since the variant data is of a different type for each event.
                            match evt {
                                Event::ExpectedDisplayFrameStart(ExpectedDisplayFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ActualDisplayFrameStart(ActualDisplayFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ExpectedSurfaceFrameStart(ExpectedSurfaceFrameStart {
                                    pid,
                                    ..
                                })
                                | Event::ActualSurfaceFrameStart(ActualSurfaceFrameStart {
                                    pid,
                                    ..
                                }) => {
                                    pid.as_mut().map(|pid| {
                                        *pid = self.map_to_koid_val(*pid);
                                    });
                                }
                                Event::FrameEnd(_frame_end) => {}
                            }
                        }
                    }
                    trace_packet::Data::FtraceEvents(ftrace_bundle) => {
                        for evt in &mut ftrace_bundle.event {
                            if let Some(ref mut pid) = evt.pid {
                                *pid = self.map_thread_to_koid_val(*pid as i32) as u32;
                            }
                            if let Some(ref mut event_data) = evt.event {
                                match event_data {
                                    Print(print) => {
                                        if let Some(ref mut data) = print.buf {
                                            *data = self.map_print_event(data)
                                        }
                                    }
                                    _ => (),
                                }
                            }
                        }
                    }
                    // No need to process other data; we only fixup data that references the pid.
                    _ => (),
                }
            }
        }
        Ok(encode_trace(&proto))
    }

    fn map_print_event(&mut self, data: &String) -> String {
        if let Some(mut event) = atrace::ATraceEvent::parse(&data) {
            match event {
                atrace::ATraceEvent::Begin { ref mut pid, .. }
                | atrace::ATraceEvent::End { ref mut pid }
                | atrace::ATraceEvent::Instant { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncBegin { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncEnd { ref mut pid, .. }
                | atrace::ATraceEvent::Counter { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncTrackBegin { ref mut pid, .. }
                | atrace::ATraceEvent::AsyncTrackEnd { ref mut pid, .. }
                | atrace::ATraceEvent::Track { ref mut pid, .. } => {
                    *pid = self.map_to_koid_val(*pid as i32) as u64
                }
            }
            event.data()
        } else {
            data.to_string()
        }
    }

    fn map_thread_to_koid_val(&mut self, pid: i32) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.resolve_tid_to_koid(pid))
            .map(|k| k.raw_koid() as i32)
            .unwrap_or(pid)
    }

    fn map_to_koid_val(&mut self, pid: i32) -> i32 {
        self.session
            .as_ref()
            .and_then(|s| s.resolve_pid_to_koid(pid))
            .map(|k| k.raw_koid() as i32)
            .unwrap_or(pid)
    }
}

pub fn start_perfetto_consumer_thread(kernel: &Kernel, socket_path: FsString) -> Result<(), Errno> {
    let (mut tx, mut rx) = futures::channel::mpsc::channel::<TraceState>(32);

    // Register the observer here, before the worker below reads `trace_state()` to find a session
    // that started earlier (for example on boot). trace-engine's observer protocol is to register
    // first and then read the state (see `trace_register_observer()` in instrumentation.h), so
    // that every state change after that read is reported. This holds because registration takes
    // trace-engine's engine mutex, which `trace_engine_start()` holds from storing Started until
    // it has made the context count nonzero and notified observers. So if a start begins after
    // registration, this observer gets its start notification. If it finished before
    // registration, the worker sees the nonzero count, and then `ProlongedContext::acquire()`
    // fails only for a session that has since ended, which this observer reports. The observer
    // clears its event before it reads the state, so if it reads Started before the count is
    // nonzero, the start notification signals it again.
    //
    // One session's Started can also be reported twice: by the worker's read and by the start
    // notification, if the start falls between registration and that read, or by two observer
    // reads, if the observer was woken earlier and reads Started before the notification comes.
    // `CallbackState::on_state_change()` ignores a Started while it holds the prolonged context.
    let observer = TraceObserver::new();

    // Listens for trace state changes and sends them to Perfetto consumer thread.
    // Unlike the perfetto thread, it won't block, so we can spawn it on the main async executor.
    kernel.kthreads.spawn_future(
        move || async move {
            while let Ok(state) = observer.on_state_changed().await {
                if let Err(e) = tx.send(state).await {
                    log_error!("perfetto-trace-observer failed to send trace state change: {:?}. Receiver dropped.", e);
                    return;
                }
            }
        },
        "perfetto-trace-observer",
    );

    // Perfetto consumer task: reads state changes from the channel and handles them.
    // This task can block, so we spawn it on a dedicated thread to not block the observer or the
    // main async executor.
    let worker_closure = async move |current_task: &CurrentTask| {
        let mut callback_state = CallbackState::new(socket_path);

        fn handle_state_change(
            callback_state: &mut CallbackState,
            current_task: &&CurrentTask,
            state: TraceState,
        ) -> Result<(), anyhow::Error> {
            let current_task = current_task;
            // TODO: https://fxbug.dev/457381697 - Revise how this kernel-internal work is security-
            // checked.
            let creds = security::creds_start_internal_operation(current_task);
            current_task
                .override_creds(creds, || callback_state.on_state_change(state, current_task))
        }

        // Check for tracing already started before we began observing.
        // This happens when tracing is started on boot.
        let mut state = trace_state();
        if trace_state() == TraceState::Started {
            const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
            // When we do boot tracing, it is possible (even likely), that starnix has started but
            // perfetto may not be ready to be connected to yet.
            // In that case poll until it has started.
            loop {
                match handle_state_change(&mut callback_state, &current_task, state) {
                    Ok(_) => break, // Success, exit loop.
                    Err(e) => {
                        if let Some(errno) = e.downcast_ref::<Errno>() {
                            if errno == &starnix_uapi::errors::ENOENT
                                || errno == &starnix_uapi::errors::ECONNREFUSED
                            {
                                log_warn!(
                                    "perfetto_consumer initial start tracing failed because perfetto socket connection not established: {e:?} retrying in 5 seconds..."
                                );
                                std::thread::sleep(RETRY_DELAY);
                                // `prev_state` stays at the failed Started, so if the session
                                // is stopping by now, the retry handles its Stopping quietly.
                                callback_state.connection = None;
                                state = trace_state();
                                continue; // Retry
                            }
                        }
                        // For any other error, log and exit loop.
                        log_error!(
                            "perfetto_consumer initial start tracing failed with error: {e:?}"
                        );
                        break;
                    }
                }
            }
        }

        while let Some(state) = rx.next().await {
            handle_state_change(&mut callback_state, &current_task, state).unwrap_or_else(|e| {
                log_error!("perfetto_consumer state change callback error: {:?}", e);
            })
        }
    };
    let worker_req = SpawnRequestBuilder::new()
        .with_debug_name("perfetto-consumer")
        .with_async_closure(worker_closure)
        .build();
    kernel.kthreads.spawner().spawn_from_request(worker_req);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TraceSession, lock_trace_engine, test_bytes};
    use starnix_core::testing::spawn_kernel_and_run;
    use std::collections::VecDeque;

    /// A `BlobSink` that returns scripted write results.
    struct FakeSink {
        /// Results for the next attempts. Once empty, every attempt returns `default_result`.
        scripted_results: VecDeque<WriteAttempt>,
        default_result: WriteAttempt,
        attempts: usize,
        written: Vec<Vec<u8>>,
    }

    impl FakeSink {
        fn new(scripted_results: &[WriteAttempt], default_result: WriteAttempt) -> Self {
            Self {
                scripted_results: scripted_results.iter().copied().collect(),
                default_result,
                attempts: 0,
                written: Vec::new(),
            }
        }
    }

    impl BlobSink for FakeSink {
        fn try_write_blob(&mut self, blob_data: &[u8]) -> WriteAttempt {
            self.attempts += 1;
            let result = self.scripted_results.pop_front().unwrap_or(self.default_result);
            if result == WriteAttempt::Wrote {
                self.written.push(blob_data.to_vec());
            }
            result
        }
    }

    #[fuchsia::test]
    fn forward_packet_splits_packet_into_chunks() {
        let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE + 10);
        let mut sink = FakeSink::new(&[], WriteAttempt::Wrote);

        assert_eq!(
            forward_packet(&mut sink, true, &data),
            Some(data.len()),
            "forward_packet(len {})",
            data.len()
        );

        let lens: Vec<usize> = sink.written.iter().map(Vec::len).collect();
        assert_eq!(
            lens,
            vec![MAX_BLOB_PAYLOAD_SIZE, MAX_BLOB_PAYLOAD_SIZE, 10],
            "forward_packet(len {}) blob lengths",
            data.len()
        );
        assert_eq!(sink.written.concat(), data, "forward_packet(len {}) bytes", data.len());
    }

    #[fuchsia::test]
    fn forward_packet_chunk_boundaries() {
        let cases: [(usize, &[usize]); 4] = [
            (0, &[]),
            (MAX_BLOB_PAYLOAD_SIZE, &[MAX_BLOB_PAYLOAD_SIZE]),
            (MAX_BLOB_PAYLOAD_SIZE + 1, &[MAX_BLOB_PAYLOAD_SIZE, 1]),
            (2 * MAX_BLOB_PAYLOAD_SIZE, &[MAX_BLOB_PAYLOAD_SIZE; 2]),
        ];
        for (len, expected_lens) in cases {
            let data = test_bytes(len);
            let mut sink = FakeSink::new(&[], WriteAttempt::Wrote);

            assert_eq!(
                forward_packet(&mut sink, true, &data),
                Some(len),
                "forward_packet(len {len})"
            );

            assert_eq!(sink.attempts, expected_lens.len(), "forward_packet(len {len}) attempts");
            let lens: Vec<usize> = sink.written.iter().map(Vec::len).collect();
            assert_eq!(lens, expected_lens, "forward_packet(len {len}) blob lengths");
            assert_eq!(sink.written.concat(), data, "forward_packet(len {len}) bytes");
        }
    }

    #[fuchsia::test]
    fn forward_packet_waits_for_streaming_buffer_space() {
        // `forward_blob()` sleeps on the real clock for 100 ms after each full attempt, so this
        // test takes at least 200 ms.
        // TODO(b/571694521): Use a fake clock (fxr/1881109).
        let data = test_bytes(10);
        let mut sink = FakeSink::new(&[WriteAttempt::BufferFull; 2], WriteAttempt::Wrote);
        let start = std::time::Instant::now();

        assert_eq!(
            forward_packet(&mut sink, true, &data),
            Some(data.len()),
            "packet after 2 full attempts"
        );

        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "packet after 2 full attempts: waited {elapsed:?}"
        );
        assert_eq!(sink.attempts, 3, "packet after 2 full attempts");
        assert_eq!(sink.written, vec![data], "packet after 2 full attempts");
    }

    #[fuchsia::test]
    fn forward_packet_stops_at_a_blob_that_cannot_be_written() {
        // Only a full buffer with `retry_on_full` is retried. `TraceBufferSink` returns `TooLong`
        // only for a blob that does not fit in one record, which `forward_packet()` never
        // passes, so the fake stands in for it here to check that `forward_blob()` drops such a
        // blob instead of retrying it.
        let cases = [
            (WriteAttempt::BufferFull, false),
            (WriteAttempt::TracingStopped, true),
            (WriteAttempt::TooLong, true),
        ];
        for (result, retry_on_full) in cases {
            let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE + 10);
            // Every attempt after the failed one would succeed.
            let mut sink = FakeSink::new(&[WriteAttempt::Wrote, result], WriteAttempt::Wrote);
            let case = format!("second chunk got {result:?}, retry_on_full {retry_on_full}");

            assert_eq!(forward_packet(&mut sink, retry_on_full, &data), None, "{case}");

            assert_eq!(sink.attempts, 2, "{case}: no retry, and no attempt for the third chunk");
            assert_eq!(
                sink.written,
                vec![data[..MAX_BLOB_PAYLOAD_SIZE].to_vec()],
                "{case}: only the first chunk is written"
            );
        }
    }

    // The tests below run `CallbackState::on_state_change()` with and without a trace context.
    // They have no Perfetto consumer endpoint that answers EnableTracing, DisableTracing and
    // ReadBuffers, so every attempt to connect to Perfetto fails. So no test covers a Started that
    // enables Perfetto tracing and keeps the context, or a Stopping that then disables Perfetto
    // tracing, reads the buffers and releases the context. If Started stopped storing the context
    // after EnableTracing, every Stopping would skip Perfetto and no test here would fail.

    #[fuchsia::test]
    async fn started_without_trace_context_skips_perfetto() {
        spawn_kernel_and_run(async |current_task| {
            let _lock = lock_trace_engine();
            // No trace session runs in this test, so `ProlongedContext::acquire()` fails as it
            // does for a session that has already ended or hasn't finished starting. The second
            // Started is a repeat that finds no context either.
            // `started_without_trace_context_tries_again_on_next_started` covers a later Started
            // that does find one. This test doesn't cover the observer registration order in
            // `start_perfetto_consumer_thread()`.
            assert!(ProlongedContext::acquire().is_none(), "a trace session is running");
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            for state in [
                TraceState::Started,
                TraceState::Started,
                TraceState::Stopping,
                TraceState::Stopped,
            ] {
                if let Err(e) = callback_state.on_state_change(state, current_task) {
                    panic!("{state:?}: {e:?}");
                }
                assert_eq!(callback_state.prev_state, state);
                assert!(callback_state.connection.is_none(), "{state:?}: connected to Perfetto");
                assert!(callback_state.prolonged_context.is_none(), "{state:?}: holds a context");
                assert!(callback_state.session.is_none(), "{state:?}: opened a pid/koid session");
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_without_trace_context_tries_again_on_next_started() {
        spawn_kernel_and_run(async |current_task| {
            // Declared before `callback_state` so that it is dropped after it, also when a
            // failed assertion unwinds: dropping a session waits until no context is held.
            let _session;
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            {
                let _lock = lock_trace_engine();
                assert!(ProlongedContext::acquire().is_none(), "a trace session is running");
                // A Started read while the session was still starting, before trace-engine made
                // the context count nonzero.
                if let Err(e) = callback_state.on_state_change(TraceState::Started, current_task) {
                    panic!("first Started: {e:?}");
                }
                assert!(
                    callback_state.prolonged_context.is_none(),
                    "first Started: holds a context"
                );
                assert!(
                    callback_state.connection.is_none(),
                    "first Started: connected to Perfetto"
                );
            }
            // `TraceSession::start()` takes `TRACE_ENGINE_LOCK` itself, so it is released above.
            // Other tests may run in between, but they don't share `callback_state`.
            _session = TraceSession::start();
            // The session's start notification. Started now acquires a context and tries to
            // enable Perfetto tracing, which fails to connect. Only that attempt can return an
            // error, so the error shows that this Started got past `ProlongedContext::acquire()`.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "second Started didn't try to enable Perfetto tracing"
            );
            assert!(callback_state.prolonged_context.is_none(), "second Started kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn trace_context_is_held_until_stopping() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Stand in for a Started that enabled Perfetto tracing and opened a pid/koid session.
            // Enabling it for real needs a Perfetto service, which this test doesn't have.
            callback_state.prev_state = TraceState::Started;
            callback_state.prolonged_context = ProlongedContext::acquire();
            assert!(callback_state.prolonged_context.is_some(), "no trace session is running");
            callback_state.session = Some(current_task.kernel().trace_event_manager.open());
            // A repeated Started, and stale Stopped and Started states of an earlier session,
            // keep the context and the pid/koid session, and don't touch Perfetto. The test feeds
            // these states directly: the trace-engine timing that produces them (see
            // `start_perfetto_consumer_thread()`) isn't reproduced here.
            for state in
                [TraceState::Started, TraceState::Stopped, TraceState::Started, TraceState::Stopped]
            {
                if let Err(e) = callback_state.on_state_change(state, current_task) {
                    panic!("{state:?}: {e:?}");
                }
                assert!(callback_state.prolonged_context.is_some(), "{state:?}: dropped context");
                assert!(callback_state.session.is_some(), "{state:?}: dropped pid/koid session");
                assert!(callback_state.connection.is_none(), "{state:?}: connected to Perfetto");
            }
            // Stopping takes the context to disable Perfetto tracing, even after a Stopped. Here
            // it fails to connect to Perfetto, and drops the context anyway, so the trace session
            // can stop when `_session` is dropped.
            assert!(
                callback_state.on_state_change(TraceState::Stopping, current_task).is_err(),
                "Stopping didn't try to disable Perfetto tracing"
            );
            assert!(callback_state.prolonged_context.is_none(), "Stopping kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_after_stopping_cleans_up_before_starting() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Stand in for a previous session whose Stopped the observer missed, which left a
            // pid/koid session and a partial packet behind, and whose Stopping released the
            // context.
            callback_state.prev_state = TraceState::Stopping;
            callback_state.packet_data = vec![0; 8];
            callback_state.session = Some(current_task.kernel().trace_event_manager.open());
            // Started clears what is left over, then acquires a context and tries to enable
            // Perfetto tracing, which fails to connect. Only that attempt can return an error,
            // so the error shows that this Started got past `ProlongedContext::acquire()`.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "Started after Stopping didn't try to enable Perfetto tracing"
            );
            assert!(callback_state.packet_data.is_empty(), "Started kept the old packet data");
            assert!(callback_state.session.is_none(), "Started kept the old pid/koid session");
            assert!(callback_state.prolonged_context.is_none(), "Started kept the context");
        })
        .await;
    }

    #[fuchsia::test]
    async fn started_that_fails_to_enable_perfetto_keeps_no_context() {
        spawn_kernel_and_run(async |current_task| {
            let _session = TraceSession::start();
            // Nothing listens on this path, so any attempt to connect to Perfetto fails.
            let mut callback_state = CallbackState::new("/nonexistent/perfetto-consumer".into());
            // Started acquires a context, then fails to connect, so Perfetto tracing isn't
            // enabled and the context must not be kept.
            assert!(
                callback_state.on_state_change(TraceState::Started, current_task).is_err(),
                "Started connected to Perfetto"
            );
            assert!(callback_state.prolonged_context.is_none(), "Started kept the context");
            assert!(callback_state.session.is_none(), "Started opened a pid/koid session");
            // So Stopping has no Perfetto session to disable and doesn't try to connect.
            if let Err(e) = callback_state.on_state_change(TraceState::Stopping, current_task) {
                panic!("Stopping: {e:?}");
            }
            assert!(callback_state.connection.is_none(), "Stopping connected to Perfetto");
        })
        .await;
    }
}
