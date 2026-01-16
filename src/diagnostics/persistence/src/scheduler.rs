// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fetcher::{PersistenceData, ServiceData, TagData};
use crate::file_handler::{self, Timestamps};
use anyhow::{Context, anyhow, bail};
use fidl::endpoints::ControlHandle;
use futures::{StreamExt, TryStreamExt};
use hashbrown::HashMap;
use itertools::Itertools;
use log::{error, warn};
use persistence_config::{Config, ServiceName, Tag};
use std::collections::VecDeque;
use std::sync::Arc;
use {fidl_fuchsia_diagnostics as fdiagnostics, fuchsia_async as fasync};

// This contains the logic to decide which tags to fetch at what times. It contains the state of
// each tag (when last fetched, whether currently queued). When a request arrives via FIDL, it's
// sent here and results in requests queued to the Fetcher.

/// Tracks when each tag was persisted last, as necessary for implementing
/// debounce on [`TagConfig`]'s `min_seconds_between_fetch`.
#[derive(Clone)]
pub(crate) struct Scheduler;

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
    pub(crate) async fn spawn(scope: fasync::ScopeHandle, config: &Config) -> Result<(), Error> {
        if config.is_empty() {
            warn!("No config specified; skipping subscription to fuchsia.diagnostics.Sample");
            return Ok(());
        }

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
        let config = config.clone();
        scope.spawn(async move {
            let lookup = config
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

            if let Err(e) = Self::handle_sample_sink(sample_sink_stream, &lookup).await {
                error!("Error serving SampleSink: {e:?}");
            }
        });

        sample
            .commit(client_end)
            .await?
            .map_err(|e| anyhow!("failed to commit fuchsia.diagnostics.Sample config: {e:?}"))?;

        Ok(())
    }

    async fn handle_sample_sink(
        mut stream: fdiagnostics::SampleSinkRequestStream,
        lookup: &Vec<QuickTagInfo>,
    ) -> Result<(), anyhow::Error> {
        while let Some(req) =
            stream.try_next().await.context("FIDL error receiving SampleSinkRequest")?
        {
            match req {
                fdiagnostics::SampleSinkRequest::OnSampleReadied { event, control_handle } => {
                    match event {
                        fdiagnostics::SampleSinkResult::Ready(fdiagnostics::SampleReady {
                            batch_iter,
                            seconds_since_start: _,
                            __source_breaking: _,
                        }) => {
                            if let Some(iter) = batch_iter {
                                if let Err(e) = Scheduler::handle_sample_ready(iter, lookup).await {
                                    warn!("Failed to process Sample: {e:?}");
                                }
                            } else {
                                control_handle.shutdown();
                                bail!("expected BatchIterator, got None");
                            }
                        }
                        fdiagnostics::SampleSinkResult::Error(err) => {
                            control_handle.shutdown();
                            bail!("failed receiving samples: {err:?}")
                        }
                        fdiagnostics::SampleSinkResult::__SourceBreaking { unknown_ordinal } => {
                            control_handle.shutdown();
                            bail!("unknown ordinal: {unknown_ordinal}")
                        }
                    }
                }
                fdiagnostics::SampleSinkRequest::_UnknownMethod {
                    ordinal,
                    control_handle,
                    method_type,
                    ..
                } => {
                    control_handle.shutdown();
                    bail!("unknown ordinal {ordinal} (method type {method_type:?})");
                }
            }
        }
        Ok(())
    }

    async fn handle_sample_ready(
        iter: fidl::endpoints::ClientEnd<fdiagnostics::BatchIteratorMarker>,
        lookup: &Vec<QuickTagInfo>,
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

        let mut current_data = file_handler::current_data()?.unwrap_or_default();

        for tag_info in lookup {
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
