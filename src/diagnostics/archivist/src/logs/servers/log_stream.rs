// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::identity::ComponentIdentity;
use crate::logs::error::LogsError;
use crate::logs::repository::LogsRepository;
use crate::logs::shared_buffer::FxtMessage;
use fidl::endpoints::{ControlHandle, DiscoverableProtocolMarker};
use fidl_fuchsia_diagnostics::StreamMode;
use futures::{AsyncWriteExt, Stream, StreamExt};
use log::warn;
use std::pin::pin;
use std::sync::Arc;
use {fidl_fuchsia_diagnostics as fdiagnostics, fuchsia_async as fasync};

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
                        Vec::new(),
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
        logs: impl Stream<Item = FxtMessage>,
        opts: ExtendRecordOpts,
    ) {
        let mut logs = pin!(logs);
        let mut buffer = Vec::new();
        while let Some(message) = logs.next().await {
            buffer.clear();
            buffer.extend_from_slice(message.data());
            extend_fxt_record(message.component_identity(), message.dropped(), &opts, &mut buffer);
            let result = socket.write_all(&buffer).await;
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

/// Returns zero padding for `len`.
fn padding(len: usize) -> &'static [u8] {
    &[0; 8][(len + 7) % 8 + 1..]
}

pub fn extend_fxt_record(
    identity: &ComponentIdentity,
    rolled_out: u64,
    opts: &ExtendRecordOpts,
    buffer: &mut Vec<u8>,
) {
    if !opts.should_extend() {
        return;
    }

    let moniker = if opts.moniker { identity.moniker.as_ref() } else { "" };
    let component_url = if opts.component_url { identity.url.as_ref() } else { "" };
    let rolled_out_value = if opts.rolled_out { rolled_out } else { 0 };

    let moniker_len = moniker.len() as u32;
    let component_url_len = component_url.len() as u32;

    buffer.extend_from_slice(&moniker_len.to_le_bytes());
    buffer.extend_from_slice(&component_url_len.to_le_bytes());
    buffer.extend_from_slice(&rolled_out_value.to_le_bytes());

    buffer.extend_from_slice(moniker.as_bytes());
    buffer.extend_from_slice(padding(moniker.len()));

    buffer.extend_from_slice(component_url.as_bytes());
    buffer.extend_from_slice(padding(component_url.len()));
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut buffer = Vec::new();
        extend_fxt_record(&ComponentIdentity::unknown(), 42, &opts, &mut buffer);

        if !opts.should_extend() {
            assert!(buffer.is_empty());
            return;
        }

        let moniker_len = u32::from_le_bytes(buffer[0..4].try_into().unwrap()) as usize;
        let component_url_len = u32::from_le_bytes(buffer[4..8].try_into().unwrap()) as usize;

        let rolled_out = u64::from_le_bytes(buffer[8..16].try_into().unwrap());
        if opts.rolled_out {
            assert_eq!(rolled_out, expected_rolled_out);
        } else {
            assert_eq!(rolled_out, 0);
        }

        let mut offset = 16;
        let moniker = std::str::from_utf8(&buffer[offset..offset + moniker_len]).unwrap();
        assert_eq!(moniker, expected_moniker);
        let moniker_padded_len = (moniker_len + 7) & !7;
        offset += moniker_padded_len;

        let url = std::str::from_utf8(&buffer[offset..offset + component_url_len]).unwrap();
        assert_eq!(url, expected_url);
        let component_url_padded_len = (component_url_len + 7) & !7;
        offset += component_url_padded_len;

        assert_eq!(offset, buffer.len());
    }
}
