// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{Dispatcher, Incoming};
use anyhow::{Context as _, Error};
use cobalt_client::traits::AsEventCode;
use fidl_fuchsia_metrics as metrics;
use log::warn;
use metrics_registry::*;

/// Connects to the MetricEventLoggerFactory service to create a
/// MetricEventLoggerProxy for the caller.
fn create_metrics_logger(incoming: &Incoming) -> Result<metrics::MetricEventLoggerProxy, Error> {
    let factory_proxy = incoming
        .connect_protocol::<metrics::MetricEventLoggerFactoryProxy>()
        .context("connecting to metrics")?;

    let (cobalt_proxy, cobalt_server) =
        fidl::endpoints::create_proxy::<metrics::MetricEventLoggerMarker>();

    let project_spec = metrics::ProjectSpec {
        customer_id: None, // defaults to fuchsia
        project_id: Some(PROJECT_ID),
        ..Default::default()
    };

    Dispatcher::spawn_local(async move {
        match factory_proxy.create_metric_event_logger(&project_spec, cobalt_server).await {
            Err(e) => warn!("FIDL failure setting up event logger: {e:?}"),
            Ok(Err(e)) => warn!("CreateMetricEventLogger failure: {e:?}"),
            Ok(Ok(())) => {}
        }
    })
    .detach();

    Ok(cobalt_proxy)
}

fn log_on_failure(result: Result<Result<(), metrics::Error>, fidl::Error>) {
    match result {
        Ok(Ok(())) => (),
        e => warn!("failed to log metrics: {:?}", e),
    };
}

/// A client connection to the Cobalt logging service.
#[derive(Clone, Debug, Default)]
pub struct MetricsLogger(Option<metrics::MetricEventLoggerProxy>);

impl MetricsLogger {
    pub fn new(incoming: &Incoming) -> Self {
        let logger = create_metrics_logger(incoming)
            .map_err(|e| warn!("Failed to create metrics logger: {e}"))
            .ok();
        Self(logger)
    }

    /// Logs an warning occurrence metric using the Cobalt logger. Does not block execution.
    pub fn log_warn<E: AsEventCode, S: Into<String>>(&self, event_code: E, message: S) {
        log::warn!("{}", message.into());
        self.send_metric(event_code);
    }

    /// Logs an error occurrence metric using the Cobalt logger. Does not block execution.
    pub fn log_error<E: AsEventCode, S: Into<String>>(&self, event_code: E, message: S) {
        log::error!("{}", message.into());
        self.send_metric(event_code);
    }

    // send metric, does not block the execution.
    fn send_metric<E: AsEventCode>(&self, event_code: E) {
        let Some(c) = self.0.clone() else { return };
        let code = event_code.as_event_code();
        Dispatcher::spawn_local(async move {
            log_on_failure(c.log_occurrence(INPUT_PIPELINE_ERROR_METRIC_ID, 1, &[code]).await);
        })
        .detach();
    }
}
