// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fuchsia_inspect::{Node, NumericProperty, UintProperty};
use sampler_component_config::Config as ComponentConfig;
use sampler_config::runtime::ProjectConfig;
use sampler_config::{MetricType, ProjectId};
use std::collections::HashMap;

/// Container for all configurations needed to instantiate the Sampler infrastructure.
/// Includes:
///      - Project configurations.
///      - Whether to configure the ArchiveReader for tests (e.g. longer timeouts)
///      - Minimum sample rate.
#[derive(Debug)]
pub struct SamplerConfig {
    pub project_configs: Vec<ProjectConfig>,
    pub stats: SamplerStats,
}

#[derive(Debug)]
pub struct ProjectStats {
    _project_node: Node,
    pub metrics_configured: UintProperty,
    pub cobalt_logs_sent: UintProperty,
}

#[derive(Default, Debug)]
pub struct SamplerStats {
    pub projects: HashMap<ProjectId, ProjectStats>,
}

impl SamplerConfig {
    pub fn new(config: ComponentConfig, stats: &Node) -> Result<Self, Error> {
        let ComponentConfig { minimum_sample_rate_sec, project_configs } = config;
        let mut sampler_stats = SamplerStats::default();
        let project_configs = project_configs
            .into_iter()
            .map(|config| {
                let config: ProjectConfig = serde_json::from_str(&config)?;
                if config.poll_rate_sec < minimum_sample_rate_sec {
                    return Err(anyhow::anyhow!(
                        "Project {} had illegal poll rate. Actual: {}s, Min: {}s",
                        config.project_id,
                        config.poll_rate_sec,
                        minimum_sample_rate_sec
                    ));
                }
                sampler_stats
                    .projects
                    .entry(config.project_id)
                    .and_modify(|project| {
                        project.metrics_configured.add(config.metrics.len() as u64);
                    })
                    .or_insert_with(|| {
                        let project_node =
                            stats.create_child(format!("project_{}", config.project_id));
                        let metrics_configured = project_node
                            .create_uint("metrics_configured", config.metrics.len() as u64);
                        let cobalt_logs_sent = project_node.create_uint("cobalt_logs_sent", 0);
                        ProjectStats {
                            _project_node: project_node,
                            metrics_configured,
                            cobalt_logs_sent,
                        }
                    });
                Ok(config)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(Self { project_configs, stats: sampler_stats })
    }

    pub fn sample_data(&self) -> Vec<fdiagnostics::SampleDatum> {
        let mut data = vec![];
        for project in &self.project_configs {
            for metric in &project.metrics {
                let strategy = Some(match metric.metric_type {
                    MetricType::Integer | MetricType::String => {
                        fdiagnostics::SampleStrategy::Always
                    }
                    MetricType::IntHistogram | MetricType::Occurrence => {
                        fdiagnostics::SampleStrategy::OnDiff
                    }
                });

                for selector in &metric.selectors {
                    data.push(fdiagnostics::SampleDatum {
                        selector: Some(fdiagnostics::SelectorArgument::StructuredSelector(
                            selector.clone(),
                        )),
                        interval_secs: Some(project.poll_rate_sec),
                        strategy,
                        ..Default::default()
                    });
                }
            }
        }

        data
    }
}
