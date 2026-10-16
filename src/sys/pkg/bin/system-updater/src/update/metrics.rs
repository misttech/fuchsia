// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::update::{AttemptError, FetchError, PrepareError, ResolveError, StageError};
use anyhow::{Context, Error, format_err};
use cobalt_sw_delivery_registry as metrics;
use fidl_contrib::ProtocolConnector;
use fidl_contrib::protocol_connector::{ConnectedProtocol, ProtocolSender};
use fidl_fuchsia_metrics::{
    MetricEvent, MetricEventLoggerFactoryMarker, MetricEventLoggerProxy, ProjectSpec,
};
use fidl_fuchsia_update_installer_ext::{Initiator, State};
use fuchsia_cobalt_builders::MetricEventExt;
use fuchsia_component::client::connect_to_protocol;
use futures::FutureExt;
use futures::future::{self, Future};
use std::time::{Duration, SystemTime};

// See $FUCHSIA_OUT_DIR/gen/src/sys/pkg/bin/amber/cobalt_sw_delivery_registry.rs for more info.
use metrics::{
    SoftwareDeliveryMetricDimensionPhase as Phase,
    SoftwareDeliveryMetricDimensionStatusCode as StatusCode,
};

pub struct CobaltConnectedService;
impl ConnectedProtocol for CobaltConnectedService {
    type Protocol = MetricEventLoggerProxy;
    type ConnectError = Error;
    type Message = MetricEvent;
    type SendError = Error;

    fn get_protocol(&mut self) -> future::BoxFuture<'_, Result<MetricEventLoggerProxy, Error>> {
        async {
            let (logger_proxy, server_end) = fidl::endpoints::create_proxy();
            let metric_event_logger_factory =
                connect_to_protocol::<MetricEventLoggerFactoryMarker>()
                    .context("Failed to connect to fuchsia::metrics::MetricEventLoggerFactory")?;

            metric_event_logger_factory
                .create_metric_event_logger(
                    &ProjectSpec { project_id: Some(metrics::PROJECT_ID), ..Default::default() },
                    server_end,
                )
                .await?
                .map_err(|e| format_err!("Connection to MetricEventLogger refused {e:?}"))?;

            Ok(logger_proxy)
        }
        .boxed()
    }

    fn send_message<'a>(
        &'a mut self,
        protocol: &'a MetricEventLoggerProxy,
        msg: MetricEvent,
    ) -> future::BoxFuture<'a, Result<(), Error>> {
        async move {
            let fut = protocol.log_metric_events(&[msg]);
            fut.await?.map_err(|e| format_err!("Failed to log metric {e:?}"))?;
            Ok(())
        }
        .boxed()
    }
}

pub fn connect_to_cobalt() -> (Client, impl Future<Output = ()>) {
    let (cobalt, cobalt_fut) =
        ProtocolConnector::new(CobaltConnectedService).serve_and_log_errors();
    (Client(cobalt), cobalt_fut)
}

fn error_to_status_code(error: &fidl_fuchsia_pkg_ext::ResolveError) -> StatusCode {
    match *error {
        fidl_fuchsia_pkg_ext::ResolveError::Io => StatusCode::ErrorStorage,
        fidl_fuchsia_pkg_ext::ResolveError::NoSpace => StatusCode::ErrorStorageOutOfSpace,
        fidl_fuchsia_pkg_ext::ResolveError::AccessDenied => StatusCode::ErrorUntrustedTufRepo,
        fidl_fuchsia_pkg_ext::ResolveError::UnavailableBlob => StatusCode::ErrorNetworking,
        _ => StatusCode::Error,
    }
}

fn result_to_status_code<T>(res: Result<T, &AttemptError>) -> StatusCode {
    match res {
        Ok(_) => StatusCode::Success,
        Err(
            AttemptError::Prepare(PrepareError::ResolveUpdate(ResolveError::Error(error, _)))
            | AttemptError::Stage(StageError::Resolve(ResolveError::Error(error, _)))
            | AttemptError::Fetch(FetchError::Resolve(ResolveError::Error(error, _)))
            | AttemptError::Fetch(FetchError::FetchBlob(error))
            | AttemptError::Stage(StageError::FetchBlob(error)),
        ) => error_to_status_code(error),
        Err(AttemptError::UpdateCanceled) => StatusCode::Canceled,
        // Fallback to a generic catch-all error status code when the error didn't contain
        // context indicating more clearly what type of error happened.
        Err(_) => StatusCode::Error,
    }
}

fn state_to_phase(state: &State) -> u32 {
    let phase = match state {
        State::Prepare | State::FailPrepare(_) => Phase::Tufupdate,
        State::Stage(_) | State::FailStage(_) => Phase::ImageWrite,
        State::Fetch(_) | State::FailFetch(_) => Phase::PackageDownload,
        State::Commit(_) | State::FailCommit(_) => Phase::ImageCommit,
        State::WaitToReboot(_)
        | State::Reboot(_)
        | State::DeferReboot(_)
        | State::CompleteButNotActive(_) => Phase::SuccessPendingReboot,
        State::Canceled => {
            log::error!("Unexpected Canceled state when logging metrics");
            return 0;
        }
    };
    phase as u32
}

fn hour_of_day(when: SystemTime) -> u32 {
    use chrono::Timelike;

    let when = chrono::DateTime::<chrono::Utc>::from(when);
    // TODO insert UTC to local time conversion here...when that is possible to do.
    when.hour()
}

fn duration_to_micros(duration: Duration) -> i64 {
    duration.as_micros().try_into().unwrap_or(i64::MAX)
}

#[derive(Debug, Clone)]
pub struct Client(ProtocolSender<MetricEvent>);

impl Client {
    pub fn log_ota_start(&mut self, initiator: Initiator, when: SystemTime) {
        self.0.send(
            MetricEvent::builder(metrics::OTA_START_MIGRATED_METRIC_ID)
                .with_event_codes((initiator, hour_of_day(when)))
                .as_occurrence(1),
        );
    }

    pub(super) fn log_ota_result<T>(
        &mut self,
        initiator: Initiator,
        attempts: u32,
        last_state: &State,
        duration: Duration,
        res: Result<T, &AttemptError>,
    ) {
        let phase = state_to_phase(last_state);
        let status = result_to_status_code(res);
        self.0.send(
            MetricEvent::builder(metrics::OTA_RESULT_ATTEMPTS_MIGRATED_METRIC_ID)
                .with_event_codes((initiator, phase, status))
                .as_occurrence(attempts.into()),
        );
        self.0.send(
            MetricEvent::builder(metrics::OTA_RESULT_DURATION_MIGRATED_METRIC_ID)
                .with_event_codes((initiator, phase, status))
                .as_integer(duration_to_micros(duration)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::prelude::*;

    #[test]
    fn hour_of_day_returns_hour_of_day() {
        assert_eq!(hour_of_day(Utc.with_ymd_and_hms(1971, 1, 1, 4, 0, 0).unwrap().into()), 4);
        assert_eq!(hour_of_day(Utc.with_ymd_and_hms(1971, 1, 1, 23, 0, 0).unwrap().into()), 23);
    }
}
