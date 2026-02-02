// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::BUILD_CONFIG;
use crate::fetcher::{PersistenceData, ServiceData, TagData};
use crate::file_handler::{self, Timestamps};
use anyhow::{Context, anyhow, bail};
use fidl::endpoints::ControlHandle;
use futures::{StreamExt, TryStreamExt};
use hashbrown::HashMap;
use itertools::Itertools;
use log::{debug, error, warn};

use persistence_config::{Config, ServiceName, Tag};
use std::collections::VecDeque;
use std::pin::pin;
use std::sync::Arc;
use {fidl_fuchsia_diagnostics as fdiagnostics, fuchsia_async as fasync};

// This contains the logic to configure the Archivist to sample diagnostics data based on the
// persistence configuration. It handles the `fuchsia.diagnostics.SampleSink` protocol to receive
// signals when data is ready, reads the data, and persists it.

/// Tracks when each tag was persisted last, as necessary for implementing
/// debounce on [`TagConfig`]'s `min_seconds_between_fetch`.
#[derive(Clone)]
pub(crate) struct Scheduler {
    /// Flat lookup table for corresponding OnSampleReady responses with
    /// Persistence configured services/tags.
    tag_info: Arc<Vec<QuickTagInfo>>,
}

/// Scheduler error.
#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("invalid selector")]
    InvalidSelector(#[from] selectors::Error),
    #[error("fidl error: {0:?}")]
    Fidl(#[from] fidl::Error),
    #[error("unable to configure Archivist sampling: {0:?}")]
    UnableToSample(#[from] anyhow::Error),
}

impl Scheduler {
    pub(crate) fn new(config: &Config) -> Self {
        let tag_info = config
            .clone()
            .into_iter()
            .flat_map(|(service, tags)| {
                tags.into_iter().map(move |(tag, config)| QuickTagInfo {
                    service: service.clone(),
                    tag,
                    max_bytes: config.max_bytes,
                    selectors: config.selectors,
                })
            })
            .collect::<Vec<_>>();

        Self { tag_info: Arc::new(tag_info) }
    }

    pub(crate) async fn subscribe(
        &self,
        scope: fasync::ScopeHandle,
        config: &Config,
    ) -> Result<(), Error> {
        let sample_datums = config
            .values()
            .flat_map(|tags| {
                tags.values().flat_map(|tag| {
                    tag.selectors
                        .clone()
                        .into_iter()
                        .map(|selector| (selector, tag.min_seconds_between_fetch))
                })
            })
            // Convert to SampleDatums.
            .map(|(selector, min_seconds_between_fetch)| fdiagnostics::SampleDatum {
                selector: Some(fdiagnostics::SelectorArgument::StructuredSelector(selector)),
                strategy: Some(fdiagnostics::SampleStrategy::OnDiff),
                interval_secs: Some(min_seconds_between_fetch),
                ..Default::default()
            })
            .collect::<Vec<_>>();

        if sample_datums.is_empty() {
            warn!("No tags configured; skipping subscription to fuchsia.diagnostics.Sample");
            return Ok(());
        }

        let sample =
            fuchsia_component::client::connect_to_protocol::<fdiagnostics::SampleMarker>()?;

        for chunk in sample_datums.chunks(fdiagnostics::MAX_SAMPLE_PARAMETERS_PER_SET as usize) {
            sample.set(&fdiagnostics::SampleParameters {
                data: Some(chunk.to_vec()),
                ..Default::default()
            })?;
        }

        let (client_end, sample_sink_stream) =
            fidl::endpoints::create_request_stream::<fdiagnostics::SampleSinkMarker>();

        // Start the SampleSink server before committing Sample configuration.
        // Dropping the JoinHandle will detach it.
        let scheduler = self.clone();
        scope.spawn(async move {
            if let Err(e) = scheduler.handle_sample_sink(sample_sink_stream).await {
                error!("Error serving SampleSink: {e:?}");
            }
        });

        sample
            .commit(client_end)
            .await?
            .map_err(|e| anyhow!("failed to commit fuchsia.diagnostics.Sample config: {e:?}"))?;

        Ok(())
    }

    pub(crate) async fn handle_sample_sink(
        &self,
        stream: fdiagnostics::SampleSinkRequestStream,
    ) -> Result<(), anyhow::Error> {
        let (stream, stalled) = detect_stall::until_stalled(stream, BUILD_CONFIG.stall_interval);
        let mut stream = pin!(stream);
        loop {
            match stream.try_next().await {
                Ok(Some(fdiagnostics::SampleSinkRequest::OnSampleReadied {
                    event,
                    control_handle,
                })) => match event {
                    fdiagnostics::SampleSinkResult::Ready(fdiagnostics::SampleReady {
                        batch_iter,
                        seconds_since_start: _,
                        __source_breaking: _,
                    }) => {
                        if let Some(iter) = batch_iter {
                            if let Err(e) = self.handle_sample_ready(iter).await {
                                warn!("Failed to process Sample: {e:?}");
                            }
                        } else {
                            bail!("expected BatchIterator, got None");
                        }
                    }
                    fdiagnostics::SampleSinkResult::Error(err) => {
                        control_handle.shutdown();
                        bail!("Failed receiving samples: {err:?}");
                    }
                    fdiagnostics::SampleSinkResult::__SourceBreaking { unknown_ordinal } => {
                        control_handle.shutdown();
                        bail!("unknown ordinal {unknown_ordinal}");
                    }
                },
                Ok(Some(req)) => {
                    warn!("Unknown SampleSinkRequest {req:?}");
                }
                Ok(None) => break,
                Err(e) => bail!("Unexpected error handling SampleSinkRequest: {e}"),
            }
        }
        if let Some(server_end) = stalled.await.context("FIDL error")? {
            // Send the server endpoint back to the framework.
            debug!("Escrowing fuchsia.diagnostics.SampleSink");
            fuchsia_component::client::connect_channel_to_protocol_at_path(
                server_end,
                "/escrow/fuchsia.diagnostics.SampleSink",
            )
            .context("Failed to connect to fuchsia.diagnostics.SampleSink")?;
        }
        Ok(())
    }

    async fn handle_sample_ready(
        &self,
        iter: fidl::endpoints::ClientEnd<fdiagnostics::BatchIteratorMarker>,
    ) -> Result<(), anyhow::Error> {
        let timestamps = file_handler::Timestamps {
            last_sample_boot: zx::BootInstant::get(),
            last_sample_utc: fuchsia_runtime::utc_time(),
        };

        let proxy = Arc::new(iter.into_proxy());
        let (snapshot, errs): (Vec<_>, Vec<_>) =
            diagnostics_reader::drain_batch_iterator::<diagnostics_data::InspectData>(proxy)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .partition_result();
        if !errs.is_empty() {
            if snapshot.is_empty() {
                bail!("failed reading all Inspect data: {errs:?}")
            } else {
                warn!("failed reading some Inspect data: {errs:?}")
            }
        }

        let mut current_data = file_handler::current_data().await?.unwrap_or_default();

        for tag_info in self.tag_info.iter() {
            for data in snapshot.clone() {
                match data.filter(&tag_info.selectors) {
                    Ok(Some(data)) => {
                        modify_tag_data(&mut current_data, tag_info, &timestamps, |tag_data| {
                            tag_data.merge(timestamps.clone(), data)
                        })
                    }
                    Ok(None) => {}
                    Err(e) => {
                        modify_tag_data(&mut current_data, tag_info, &timestamps, |tag_data| {
                            tag_data.add_error(e.to_string())
                        })
                    }
                }
            }
        }

        file_handler::write_current_data(&current_data)
            .context("Failed to write current data to disk")
    }
}

fn modify_tag_data<'a>(
    data: &'a mut PersistenceData,
    lookup: &'a QuickTagInfo,
    timestamps: &Timestamps,
    modify_fn: impl FnOnce(&mut TagData),
) {
    let QuickTagInfo { service, tag, max_bytes, selectors } = lookup;

    let service_data = data.entry_ref(service).or_insert_with(ServiceData::default);
    let tag_data = service_data.entry_ref(tag).or_insert_with(|| TagData {
        max_bytes: *max_bytes,
        total_bytes: 0,
        timestamps: timestamps.clone(),
        selectors: selectors.clone(),
        data: HashMap::new(),
        errors: VecDeque::new(),
    });

    modify_fn(tag_data)
}

/// Minimal set of information to perform quick lookups of tags.
struct QuickTagInfo {
    service: ServiceName,
    tag: Tag,
    max_bytes: usize,
    selectors: Vec<fidl_fuchsia_diagnostics::Selector>,
}
