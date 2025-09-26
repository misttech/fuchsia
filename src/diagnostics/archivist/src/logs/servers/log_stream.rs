// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::identity::ComponentIdentity;
use crate::logs::container::CursorItem;
use crate::logs::error::LogsError;
use crate::logs::repository::LogsRepository;
use fidl::endpoints::{ControlHandle, DiscoverableProtocolMarker};
use fidl_fuchsia_diagnostics::StreamMode;
use futures::{AsyncWriteExt, Stream, StreamExt};
use log::warn;
use std::borrow::Cow;
use std::sync::Arc;
use {fidl_fuchsia_diagnostics as fdiagnostics, fuchsia_async as fasync, fuchsia_trace as ftrace};

pub struct LogStreamServer {
    /// The repository holding the logs.
    logs_repo: Arc<LogsRepository>,

    /// Scope in which we spawn all of the server tasks.
    scope: fasync::Scope,
}

impl LogStreamServer {
    pub fn new(logs_repo: Arc<LogsRepository>, scope: fasync::Scope) -> Self {
        Self { logs_repo, scope }
    }

    /// Spawn a task to handle requests from components reading the shared log.
    pub fn spawn(&self, stream: fdiagnostics::LogStreamRequestStream) {
        let logs_repo = Arc::clone(&self.logs_repo);
        let scope = self.scope.to_handle();
        self.scope.spawn(async move {
            if let Err(e) = Self::handle_requests(logs_repo, stream, scope).await {
                warn!("error handling Log requests: {}", e);
            }
        });
    }

    /// Handle requests to `fuchsia.diagnostics.LogStream`. All request types read the
    /// whole backlog from memory, `DumpLogs(Safe)` stops listening after that.
    async fn handle_requests(
        logs_repo: Arc<LogsRepository>,
        mut stream: fdiagnostics::LogStreamRequestStream,
        scope: fasync::ScopeHandle,
    ) -> Result<(), LogsError> {
        while let Some(request) = stream.next().await {
            let request = request.map_err(|source| LogsError::HandlingRequests {
                protocol: fdiagnostics::LogStreamMarker::PROTOCOL_NAME,
                source,
            })?;

            match request {
                fdiagnostics::LogStreamRequest::Connect { socket, opts, .. } => {
                    let logs = logs_repo.logs_cursor_raw(
                        opts.mode.unwrap_or(StreamMode::SnapshotThenSubscribe),
                        None,
                        ftrace::Id::random(),
                    );
                    let opts = ExtendRecordOpts::from(opts);
                    scope.spawn(Self::stream_logs(fasync::Socket::from_socket(socket), logs, opts));
                }
                fdiagnostics::LogStreamRequest::_UnknownMethod {
                    ordinal,
                    method_type,
                    control_handle,
                    ..
                } => {
                    warn!(ordinal, method_type:?; "Unknown request. Closing connection");
                    control_handle.shutdown_with_epitaph(zx::Status::UNAVAILABLE);
                }
            }
        }
        Ok(())
    }

    async fn stream_logs(
        mut socket: fasync::Socket,
        mut logs: impl Stream<Item = CursorItem> + Unpin,
        opts: ExtendRecordOpts,
    ) {
        while let Some(CursorItem { rolled_out, message, identity }) = logs.next().await {
            let response = extend_fxt_record(message.bytes(), identity.as_ref(), rolled_out, &opts);
            let result = socket.write_all(&response).await;
            if result.is_err() {
                // Assume an error means the peer closed for now.
                break;
            }
        }
    }
}

#[derive(Default)]
pub struct ExtendRecordOpts {
    pub moniker: bool,
    pub component_url: bool,
    pub rolled_out: bool,
}

impl ExtendRecordOpts {
    fn should_extend(&self) -> bool {
        let Self { moniker, component_url, rolled_out } = self;
        *moniker || *component_url || *rolled_out
    }
}

impl From<fdiagnostics::LogStreamOptions> for ExtendRecordOpts {
    fn from(opts: fdiagnostics::LogStreamOptions) -> Self {
        let fdiagnostics::LogStreamOptions {
            include_moniker,
            include_component_url,
            include_rolled_out,
            mode: _,
            __source_breaking: _,
        } = opts;
        Self {
            moniker: include_moniker.unwrap_or(false),
            component_url: include_component_url.unwrap_or(false),
            rolled_out: include_rolled_out.unwrap_or(false),
        }
    }
}

/// Calculates the smallest multiple of 8 that is greater than or equal to `len`.
fn pad_to_8(len: usize) -> usize {
    (len + 7) & !7
}

pub fn extend_fxt_record<'a>(
    fxt_record: &'a [u8],
    identity: &ComponentIdentity,
    rolled_out: u64,
    opts: &ExtendRecordOpts,
) -> Cow<'a, [u8]> {
    if !opts.should_extend() {
        return Cow::Borrowed(fxt_record);
    }

    let moniker_str = if opts.moniker { Some(identity.moniker.to_string()) } else { None };
    let moniker = moniker_str.as_deref().unwrap_or("");
    let component_url = if opts.component_url { identity.url.as_ref() } else { "" };
    let rolled_out_value = if opts.rolled_out { rolled_out } else { 0 };

    let moniker_len = moniker.len() as u32;
    let component_url_len = component_url.len() as u32;

    let moniker_padded_len = pad_to_8(moniker_len as usize);
    let component_url_padded_len = pad_to_8(component_url_len as usize);

    let mut extended_buffer =
        Vec::with_capacity(fxt_record.len() + 16 + moniker_padded_len + component_url_padded_len);
    extended_buffer.extend_from_slice(fxt_record);

    extended_buffer.extend_from_slice(&moniker_len.to_le_bytes());
    extended_buffer.extend_from_slice(&component_url_len.to_le_bytes());
    extended_buffer.extend_from_slice(&rolled_out_value.to_le_bytes());

    extended_buffer.extend_from_slice(moniker.as_bytes());

    // These resize operations are needed because the bytes in component_url and
    // moniker do not include padding, so we need to pad the end with zeroes.
    extended_buffer.resize(extended_buffer.len() + moniker_padded_len - moniker_len as usize, 0);

    extended_buffer.extend_from_slice(component_url.as_bytes());
    extended_buffer
        .resize(extended_buffer.len() + component_url_padded_len - component_url_len as usize, 0);

    Cow::Owned(extended_buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_log_encoding::Argument;
    use diagnostics_log_encoding::encode::{
        Encoder, EncoderOpts, MutableBuffer, TestRecord, WriteEventParams,
    };
    use diagnostics_log_encoding::parse::parse_record;
    use diagnostics_log_types::Severity;
    use std::io::Cursor;
    use test_case::test_case;

    #[test_case(ExtendRecordOpts::default(), "", "", 0 ; "no_additional_metadata")]
    #[test_case(
        ExtendRecordOpts { moniker: true, ..Default::default() },
        "UNKNOWN",
        "",
        0
        ; "with_moniker")]
    #[test_case(
        ExtendRecordOpts { component_url: true, ..Default::default() },
        "",
        "fuchsia-pkg://UNKNOWN",
        0
        ; "with_url")]
    #[test_case(
        ExtendRecordOpts { rolled_out: true, ..Default::default() },
        "",
        "",
        42
        ; "with_rolled_out")]
    #[test_case(
        ExtendRecordOpts { moniker: true, component_url: true, rolled_out: true },
        "UNKNOWN",
        "fuchsia-pkg://UNKNOWN",
        42
        ; "with_all")]
    #[fuchsia::test]
    fn extend_record_with_metadata(
        opts: ExtendRecordOpts,
        expected_moniker: &str,
        expected_url: &str,
        expected_rolled_out: u64,
    ) {
        let mut encoder = Encoder::new(Cursor::new([0u8; 4096]), EncoderOpts::default());
        encoder
            .write_event(WriteEventParams::<_, &str, _> {
                event: TestRecord {
                    severity: Severity::Warn as u8,
                    timestamp: zx::BootInstant::from_nanos(1234567890),
                    file: Some("foo.rs"),
                    line: Some(123),
                    record_arguments: vec![Argument::tag("hello"), Argument::message("testing")],
                },
                tags: &[],
                metatags: std::iter::empty(),
                pid: zx::Koid::from_raw(1),
                tid: zx::Koid::from_raw(2),
                dropped: 10,
            })
            .expect("wrote event");

        let length = encoder.inner().cursor();
        let original_record_bytes = &encoder.inner().get_ref()[..length];
        let (expected_record, _) = parse_record(original_record_bytes).unwrap();

        let extended_record_bytes =
            extend_fxt_record(original_record_bytes, &ComponentIdentity::unknown(), 42, &opts);

        if !opts.should_extend() {
            assert_eq!(extended_record_bytes, original_record_bytes);
            return;
        }

        let (record, rest) = parse_record(&extended_record_bytes).unwrap();
        assert_eq!(record, expected_record);

        let moniker_len = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        let component_url_len = u32::from_le_bytes(rest[4..8].try_into().unwrap()) as usize;

        let rolled_out = u64::from_le_bytes(rest[8..16].try_into().unwrap());
        if opts.rolled_out {
            assert_eq!(rolled_out, expected_rolled_out);
        } else {
            assert_eq!(rolled_out, 0);
        }

        let mut offset = 16;
        let moniker = std::str::from_utf8(&rest[offset..offset + moniker_len]).unwrap();
        assert_eq!(moniker, expected_moniker);
        let moniker_padded_len = (moniker_len + 7) & !7;
        offset += moniker_padded_len;

        let url = std::str::from_utf8(&rest[offset..offset + component_url_len]).unwrap();
        assert_eq!(url, expected_url);
        let component_url_padded_len = (component_url_len + 7) & !7;
        offset += component_url_padded_len;

        assert_eq!(offset, rest.len());
    }
}
