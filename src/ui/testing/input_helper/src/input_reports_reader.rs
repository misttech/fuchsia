// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use fidl::endpoints::RequestStream as _;
use fidl_fuchsia_input_report::{
    InputReport, InputReportsReaderRequest, InputReportsReaderRequestStream,
    InputReportsReaderV2Request, InputReportsReaderV2RequestStream,
};
use futures::{StreamExt, TryStreamExt};
use std::collections::VecDeque;
use std::convert::TryFrom as _;

/// Implements the server side of the `fuchsia.input.report.InputReportsReader`
/// protocol. Used by `modern_backend::InputDevice`.
pub(super) struct InputReportsReaderV1 {
    pub(super) request_stream: InputReportsReaderRequestStream,
    /// FIFO queue of reports to be consumed by calls to
    /// `fuchsia.input.report.InputReportsReader.ReadInputReports()`.
    pub(super) report_receiver: futures::channel::mpsc::UnboundedReceiver<InputReport>,
}

impl InputReportsReaderV1 {
    /// Returns a `Future` that resolves when
    /// * `self.reports` is empty, or
    /// * `self.request_stream` yields `None`, or
    /// * an error occurs (invalid FIDL request, failure to send FIDL response).
    ///
    /// # Resolves to
    /// * `Ok(())` if all reports were written successfully
    /// * `Err` otherwise
    ///
    /// # Corner cases
    /// If `self.reports` is _initially_ empty, the returned `Future` will resolve immediately.
    ///
    /// # Note
    /// When the future resolves, `InputReports` may still be sitting unread in the
    /// channel to the `fuchsia.input.report.InputReportsReader` client. (The client will
    /// typically be an input pipeline implementation.)
    pub(super) async fn into_future(self) -> Result<(), Error> {
        // Group `reports` into chunks, to respect the requirements of the `InputReportsReader`
        // protocol. Then `zip()` each chunk with a `InputReportsReader` protocol request.
        // * If there are more chunks than requests, then some of the `InputReport`s were
        //   not sent to the `InputReportsReader` client. In this case, this function
        //   will report an error by checking `reports.is_done()` below.
        // * If there are more requests than reports, no special-case handling is needed.
        //   This is because an input pipeline implementation will normally issue
        //   `ReadInputReports` requests indefinitely.
        let chunk_size = usize::try_from(fidl_fuchsia_input_report::MAX_DEVICE_REPORT_COUNT)
            .context("converting MAX_DEVICE_REPORT_COUNT to usize")?;
        let mut reports = self.report_receiver.ready_chunks(chunk_size).fuse();
        self.request_stream
            .zip(reports.by_ref())
            .map(|(request, reports)| match request {
                Ok(request) => Ok((request, reports)),
                Err(e) => Err(anyhow::Error::from(e).context("while reading reader request")),
            })
            .try_for_each(|request_and_reports| async {
                match request_and_reports {
                    (InputReportsReaderRequest::ReadInputReports { responder }, reports) => {
                        responder
                            .send(Ok(reports))
                            .map_err(anyhow::Error::from)
                            .context("while sending reports")
                    }
                }
            })
            .await?;

        match reports.is_done() {
            true => Ok(()),
            false => Err(format_err!("request_stream terminated with reports still pending")),
        }
    }
}

/// Implements the server side of the `fuchsia.input.report.InputReportsReaderV2`
/// protocol. Used by `modern_backend::InputDevice`.
pub(super) struct InputReportsReaderV2 {
    pub(super) request_stream: InputReportsReaderV2RequestStream,
    /// FIFO queue of reports to be pushed via `OnInputReports` events.
    pub(super) report_receiver: futures::channel::mpsc::UnboundedReceiver<InputReport>,
    pub(super) max_unacknowledged_reports: u16,
}

impl InputReportsReaderV2 {
    pub(super) async fn into_future(self) -> Result<(), Error> {
        let chunk_size = usize::try_from(fidl_fuchsia_input_report::MAX_DEVICE_REPORT_COUNT)
            .context("converting MAX_DEVICE_REPORT_COUNT to usize")?;
        let mut reports_stream = self.report_receiver.ready_chunks(chunk_size).fuse();
        let control_handle = self.request_stream.control_handle();
        let mut request_stream = self.request_stream.fuse();

        let mut last_report_stamp: u64 = 0;
        let mut last_acknowledged_report_stamp: u64 = 0;
        let mut pending_reports: VecDeque<InputReport> = VecDeque::new();

        loop {
            let unacknowledged = last_report_stamp.saturating_sub(last_acknowledged_report_stamp);
            let can_send = unacknowledged < (self.max_unacknowledged_reports as u64);

            if can_send && !pending_reports.is_empty() {
                let take_count = std::cmp::min(pending_reports.len(), chunk_size);
                let reports_batch: Vec<InputReport> = pending_reports.drain(..take_count).collect();
                last_report_stamp += reports_batch.len() as u64;
                control_handle
                    .send_on_input_reports(reports_batch, last_report_stamp)
                    .context("failed to send OnInputReports event")?;
                continue;
            }

            futures::select! {
                request = request_stream.next() => {
                    match request {
                        Some(Ok(InputReportsReaderV2Request::AcknowledgeReports {
                            last_acknowledged_report_stamp: stamp,
                            ..
                        })) => {
                            if stamp > last_acknowledged_report_stamp {
                                last_acknowledged_report_stamp = stamp;
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow::Error::from(e).context("error on V2 reader stream")),
                        None => break,
                    }
                }
                reports = reports_stream.next() => {
                    match reports {
                        Some(reports_batch) if !reports_batch.is_empty() => {
                            pending_reports.extend(reports_batch);
                        }
                        _ => break,
                    }
                }
                complete => break,
            }
        }

        Ok(())
    }
}

pub(super) enum InputReportsReader {
    V1(InputReportsReaderV1),
    V2(InputReportsReaderV2),
}

impl InputReportsReader {
    pub(super) async fn into_future(self) -> Result<(), Error> {
        match self {
            InputReportsReader::V1(v1) => v1.into_future().await,
            InputReportsReader::V2(v2) => v2.into_future().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InputReport, InputReportsReader, InputReportsReaderV1, InputReportsReaderV2};
    use anyhow::{Context as _, Error};
    use fidl::endpoints;
    use fidl_fuchsia_input_report::{InputReportsReaderMarker, MAX_DEVICE_REPORT_COUNT};
    use fuchsia_async as fasync;
    use futures::future;

    mod report_count {
        use super::*;
        use futures::pin_mut;
        use futures::task::Poll;

        #[fuchsia::test(allow_stalls = false)]
        async fn serves_single_report() -> Result<(), Error> {
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            report_sender
                .unbounded_send(InputReport::default())
                .expect("sending empty InputReport");
            let reports_fut = proxy.read_input_reports();
            std::mem::drop(proxy); // Drop `proxy` to terminate `request_stream`.
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.

            let (_, reports_result) = future::join(reader_fut, reports_fut).await;
            let reports = reports_result
                .expect("fidl error")
                .map_err(zx::Status::err_from_raw)
                .expect("service error");
            assert_eq!(reports.len(), 1, "incorrect reports length");
            Ok(())
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn serves_max_report_count_reports() -> Result<(), Error> {
            let max_reports = usize::try_from(MAX_DEVICE_REPORT_COUNT)
                .context("internal error converting MAX_DEVICE_REPORT_COUNT to usize")?;
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            for _ in 0..max_reports {
                report_sender
                    .unbounded_send(InputReport::default())
                    .expect("sending empty InputReport");
            }
            let reports_fut = proxy.read_input_reports();
            std::mem::drop(proxy); // Drop `proxy` to terminate `request_stream`.
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.

            let (_, reports_result) = future::join(reader_fut, reports_fut).await;
            let reports = reports_result
                .expect("fidl error")
                .map_err(zx::Status::err_from_raw)
                .expect("service error");
            assert_eq!(reports.len(), max_reports, "incorrect reports length");
            Ok(())
        }

        #[test]
        fn splits_overflowed_reports_to_next_read() -> Result<(), Error> {
            let mut executor = fasync::TestExecutor::new();
            let max_reports = usize::try_from(MAX_DEVICE_REPORT_COUNT)
                .context("internal error converting MAX_DEVICE_REPORT_COUNT to usize")?;
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            for _ in 0..max_reports + 1 {
                report_sender
                    .unbounded_send(InputReport::default())
                    .expect("sending empty InputReport");
            }
            pin_mut!(reader_fut);

            // Note: this test deliberately serializes its FIDL requests. Concurrent requests
            // are tested separately, in `super::fidl_interactions::preserves_query_order()`.
            let reports_fut = proxy.read_input_reports();
            let _ = executor.run_until_stalled(&mut reader_fut);
            pin_mut!(reports_fut);
            match executor.run_until_stalled(&mut reports_fut) {
                Poll::Pending => panic!("read did not complete (1st query)"),
                Poll::Ready(res) => {
                    let reports = res
                        .expect("fidl error")
                        .map_err(zx::Status::err_from_raw)
                        .expect("service error");
                    assert_eq!(reports.len(), max_reports, "incorrect reports length (1st query)");
                }
            }

            let reports_fut = proxy.read_input_reports();
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.
            let _ = executor.run_until_stalled(&mut reader_fut);
            pin_mut!(reports_fut);
            match executor.run_until_stalled(&mut reports_fut) {
                Poll::Pending => panic!("read did not complete (2nd query)"),
                Poll::Ready(res) => {
                    let reports = res
                        .expect("fidl error")
                        .map_err(zx::Status::err_from_raw)
                        .expect("service error");
                    assert_eq!(reports.len(), 1, "incorrect reports length (2nd query)");
                }
            }

            Ok(())
        }
    }

    mod future_resolution {
        use super::*;
        use assert_matches::assert_matches;

        #[fuchsia::test(allow_stalls = false)]
        async fn resolves_to_ok_when_all_reports_are_written() -> Result<(), Error> {
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            report_sender
                .unbounded_send(InputReport::default())
                .expect("sending empty InputReport");
            let _reports_fut = proxy.read_input_reports();
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.
            assert_matches!(reader_fut.await, Ok(()));
            Ok(())
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn resolves_to_err_when_request_stream_is_terminated_before_reports_are_written()
        -> Result<(), Error> {
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            report_sender
                .unbounded_send(InputReport::default())
                .expect("sending empty InputReport");
            std::mem::drop(proxy); // Drop `proxy` to terminate `request_stream`.
            assert_matches!(reader_fut.await, Err(_));
            Ok(())
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn resolves_to_err_if_request_stream_yields_error() -> Result<(), Error> {
            let (client_end, request_stream) =
                endpoints::create_request_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            report_sender
                .unbounded_send(InputReport::default())
                .expect("sending empty InputReport");
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.
            client_end
                .into_channel()
                .write(b"not a valid FIDL message", /* handles */ &mut [])
                .expect("internal error writing to channel");
            assert_matches!(reader_fut.await, Err(_)); // while reading reader request
            Ok(())
        }

        /* TODO(https://fxbug.dev/42075735): Re-enable this test
        #[fuchsia::test(allow_stalls = false)]
        async fn resolves_to_err_if_send_fails() -> Result<(), Error> {
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut = InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver }).into_future();
            report_sender.unbounded_send(InputReport::default()).expect("sending empty InputReport");
            let result_fut = proxy.read_input_reports(); // Send query.
            std::mem::drop(result_fut); // Close handle to channel.
            std::mem::drop(proxy); // Close other handle to channel.
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.
            assert_matches!(reader_fut.await, Err(_)); // while sending reports
            Ok(())
        }
        */

        #[fuchsia::test(allow_stalls = false)]
        async fn immediately_resolves_to_ok_when_reports_is_initially_empty() -> Result<(), Error> {
            let (_proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.
            assert_matches!(reader_fut.await, Ok(()));
            Ok(())
        }
    }

    mod fidl_interactions {
        use super::*;
        use assert_matches::assert_matches;
        use futures::pin_mut;
        use futures::task::Poll;

        #[test]
        fn closes_channel_after_reports_are_consumed() -> Result<(), Error> {
            let mut executor = fasync::TestExecutor::new();
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            report_sender
                .unbounded_send(InputReport::default())
                .expect("sending empty InputReport");
            let reports_fut = proxy.read_input_reports();
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.

            // Process the first query. This should close the FIDL connection.
            let futures = future::join(reader_fut, reports_fut);
            pin_mut!(futures);
            std::mem::drop(executor.run_until_stalled(&mut futures));

            // Try sending another query. This should fail.
            assert_matches!(
                executor.run_until_stalled(&mut proxy.read_input_reports()),
                Poll::Ready(Err(fidl::Error::ClientChannelClosed { .. }))
            );
            Ok(())
        }

        #[fuchsia::test(allow_stalls = false)]
        async fn preserves_query_order() -> Result<(), Error> {
            let max_reports = usize::try_from(MAX_DEVICE_REPORT_COUNT)
                .context("internal error converting MAX_DEVICE_REPORT_COUNT to usize")?;
            let (proxy, request_stream) =
                endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
            let (report_sender, report_receiver) =
                futures::channel::mpsc::unbounded::<InputReport>();
            let reader_fut =
                InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                    .into_future();
            for _ in 0..max_reports + 1 {
                report_sender
                    .unbounded_send(InputReport::default())
                    .expect("sending empty InputReport");
            }
            let first_reports_fut = proxy.read_input_reports();
            let second_reports_fut = proxy.read_input_reports();
            std::mem::drop(proxy); // Drop `proxy` to terminate `request_stream`.
            std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.

            let (_, first_reports_result, second_reports_result) =
                futures::join!(reader_fut, first_reports_fut, second_reports_fut);
            let first_reports = first_reports_result
                .expect("fidl error")
                .map_err(zx::Status::err_from_raw)
                .expect("service error");
            let second_reports = second_reports_result
                .expect("fidl error")
                .map_err(zx::Status::err_from_raw)
                .expect("service error");
            assert_eq!(first_reports.len(), max_reports, "incorrect reports length (1st query)");
            assert_eq!(second_reports.len(), 1, "incorrect reports length (2nd query)");
            Ok(())
        }
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn preserves_report_order() -> Result<(), Error> {
        let (proxy, request_stream) =
            endpoints::create_proxy_and_stream::<InputReportsReaderMarker>();
        let (report_sender, report_receiver) = futures::channel::mpsc::unbounded::<InputReport>();
        let reader_fut =
            InputReportsReader::V1(InputReportsReaderV1 { request_stream, report_receiver })
                .into_future();
        report_sender
            .unbounded_send(InputReport { event_time: Some(1), ..Default::default() })
            .expect("sending first InputReport");
        report_sender
            .unbounded_send(InputReport { event_time: Some(2), ..Default::default() })
            .expect("sending second InputReport");

        let reports_fut = proxy.read_input_reports();
        std::mem::drop(report_sender); // Drop `report_sender` to terminate `report_receiver`.

        assert_eq!(
            future::join(reader_fut, reports_fut)
                .await
                .1
                .expect("fidl error")
                .map_err(zx::Status::err_from_raw)
                .expect("service error")
                .iter()
                .map(|report| report.event_time)
                .collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        Ok(())
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn v2_serves_input_reports_and_handles_ack() -> Result<(), Error> {
        use fidl_fuchsia_input_report::InputReportsReaderV2Marker;
        use futures::StreamExt;
        let (proxy, request_stream) =
            endpoints::create_proxy_and_stream::<InputReportsReaderV2Marker>();
        let (report_sender, report_receiver) = futures::channel::mpsc::unbounded::<InputReport>();
        let reader_fut =
            InputReportsReaderV2 { request_stream, report_receiver, max_unacknowledged_reports: 2 }
                .into_future();

        report_sender
            .unbounded_send(InputReport { event_time: Some(1), ..Default::default() })
            .expect("sending first report");

        let mut event_stream = proxy.take_event_stream();
        let receive_event_fut = async move {
            let event = event_stream.next().await.expect("expected event").expect("fidl error");
            match event {
                fidl_fuchsia_input_report::InputReportsReaderV2Event::OnInputReports {
                    reports,
                    last_report_stamp,
                } => {
                    assert_eq!(reports.len(), 1);
                    assert_eq!(last_report_stamp, 1);
                    let _ = proxy.acknowledge_reports(last_report_stamp);
                }
                _ => panic!("unexpected event: {:?}", event),
            }
        };

        std::mem::drop(report_sender);
        let (reader_res, _) = futures::join!(reader_fut, receive_event_fut);
        assert_matches::assert_matches!(reader_res, Ok(()));
        Ok(())
    }
}
