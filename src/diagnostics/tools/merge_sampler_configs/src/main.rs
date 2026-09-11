// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, bail, format_err};
use argh::{ArgsInfo, FromArgs};
use cobalt_registry_proto::cobalt::CobaltRegistry;
use cobalt_registry_proto::cobalt::metric_definition::MetricType as CobaltMetricType;
use prost::Message;
use sampler_config::MetricType as SamplerMetricType;
use sampler_config::assembly::MergedSamplerConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;

const FUCHSIA_CUSTOMER_ID: u32 = 1;

/// Diagnostics config command
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
pub struct MergeConfigsCommand {
    /// paths to sampler project configs.
    #[argh(option)]
    pub project_config: Vec<PathBuf>,

    /// paths to sampler project templates.
    #[argh(option)]
    pub fire_project_template: Vec<PathBuf>,

    /// paths to sampler component configs.
    #[argh(option)]
    pub fire_component_config: Vec<PathBuf>,

    /// path to cobalt registry binary proto to validate against.
    #[argh(option)]
    pub cobalt_registry: Option<PathBuf>,

    /// path to which the result will be written.
    #[argh(option)]
    pub output: PathBuf,
}

pub fn main() -> Result<(), Error> {
    let args: MergeConfigsCommand = argh::from_env();

    let mut config = MergedSamplerConfig::default();

    for project_config_path in args.project_config {
        config.project_configs.push(read_file(project_config_path)?);
    }
    for project_template_path in args.fire_project_template {
        config.fire_project_templates.push(read_file(project_template_path)?);
    }
    for component_config_path in args.fire_component_config {
        config.fire_component_configs.push(read_file(component_config_path)?);
    }

    if let Some(cobalt_registry_path) = args.cobalt_registry {
        let registry_bytes = std::fs::read(&cobalt_registry_path).with_context(|| {
            format!("Failed to read cobalt registry from {:?}", cobalt_registry_path)
        })?;
        validate(&registry_bytes, &config)?;
    }

    write_file(args.output, config)?;

    Ok(())
}

fn validate(registry_bytes: &[u8], config: &MergedSamplerConfig) -> Result<(), Error> {
    let registry = CobaltRegistry::decode(registry_bytes)
        .context("Failed to decode CobaltRegistry protobuf")?;

    let customer =
        registry.customers.iter().find(|c| c.customer_id == FUCHSIA_CUSTOMER_ID).ok_or_else(
            || {
                format_err!(
                    "Fuchsia customer ID ({}) not found in Cobalt registry",
                    FUCHSIA_CUSTOMER_ID
                )
            },
        )?;

    let mut cobalt_projects = HashMap::new();
    for project in &customer.projects {
        let metrics_by_id: HashMap<u32, _> = project.metrics.iter().map(|m| (m.id, m)).collect();
        cobalt_projects.insert(project.project_id, metrics_by_id);
    }

    // Validate standard projects
    for project in &config.project_configs {
        let project_id = *project.project_id;
        let cobalt_metrics = cobalt_projects.get(&project_id).ok_or_else(|| {
            format_err!("Sampler project_id {} not found in Cobalt registry", project_id)
        })?;

        for dataset in &project.data_sets {
            for metric in &dataset.metrics {
                let metric_id = *metric.metric_id;
                let cobalt_metric = cobalt_metrics.get(&metric_id).ok_or_else(|| {
                    format_err!(
                        "Metric ID {} in project {} not found in Cobalt registry",
                        metric_id,
                        project_id
                    )
                })?;

                verify_metric_type(metric.metric_type, cobalt_metric.metric_type).map_err(|e| {
                    format_err!(
                        "Metric type mismatch for project {project_id} metric {metric_id}: {e}"
                    )
                })?;

                let expected_dims = cobalt_metric.metric_dimensions.len();
                let actual_dims = metric.event_codes.len();
                if actual_dims > expected_dims {
                    bail!(
                        "Dimension count mismatch for project {project_id} metric {metric_id}: \
                         Sampler config has {actual_dims} event_codes, but Cobalt defines {expected_dims} metric_dimensions"
                    );
                }
            }
        }
    }

    // Validate FIRE project templates
    for template in &config.fire_project_templates {
        let project_id = *template.project_id;
        let cobalt_metrics = cobalt_projects.get(&project_id).ok_or_else(|| {
            format_err!("FIRE template project_id {} not found in Cobalt registry", project_id)
        })?;

        for metric in &template.metrics {
            let metric_id = *metric.metric_id;
            let cobalt_metric = cobalt_metrics.get(&metric_id).ok_or_else(|| {
                format_err!(
                    "FIRE Metric ID {} in project {} not found in Cobalt registry",
                    metric_id,
                    project_id
                )
            })?;

            verify_metric_type(metric.metric_type, cobalt_metric.metric_type).map_err(|e| {
                format_err!(
                    "Metric type mismatch for FIRE project {project_id} metric {metric_id}: {e}"
                )
            })?;

            // In FIRE templates, component ID is injected as dimension 0, so event_codes.len() + 1
            let expected_dims = cobalt_metric.metric_dimensions.len();
            let actual_dims = metric.event_codes.len() + 1;
            if actual_dims > expected_dims {
                bail!(
                    "Dimension mismatch for FIRE project {project_id} metric {metric_id}: \
                     Sampler has {} (+1 for component) = {actual_dims}, Cobalt expects {expected_dims}",
                    metric.event_codes.len()
                );
            }
        }
    }

    Ok(())
}

fn verify_metric_type(sampler_type: SamplerMetricType, cobalt_type_raw: i32) -> Result<(), Error> {
    let cobalt_type =
        CobaltMetricType::try_from(cobalt_type_raw).unwrap_or(CobaltMetricType::Unset);
    let expected = match sampler_type {
        SamplerMetricType::Occurrence => CobaltMetricType::Occurrence,
        SamplerMetricType::Integer => CobaltMetricType::Integer,
        SamplerMetricType::IntHistogram => CobaltMetricType::IntegerHistogram,
        SamplerMetricType::String => CobaltMetricType::String,
    };
    if cobalt_type != expected {
        bail!("Sampler specified {:?}, Cobalt defines {:?}", sampler_type, cobalt_type);
    }
    Ok(())
}

fn read_file<T: DeserializeOwned>(path: PathBuf) -> anyhow::Result<T> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let result: T = serde_json5::from_reader(&mut reader)?;
    Ok(result)
}

fn write_file<T: Serialize>(path: PathBuf, value: T) -> anyhow::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json5::to_writer(&mut writer, &value)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cobalt_registry_proto::cobalt::metric_definition::MetricDimension;
    use cobalt_registry_proto::cobalt::{
        CustomerConfig, MetricDefinition, ProjectConfig as CobaltProjectConfig,
    };
    use sampler_config::assembly::MetricTemplate;
    use sampler_config::runtime::{
        DataSetConfig, MetricConfig, ProjectConfig as SamplerProjectConfig,
    };
    use sampler_config::{EventCode, MetricId, ProjectId};

    fn make_test_registry() -> CobaltRegistry {
        CobaltRegistry {
            customers: vec![CustomerConfig {
                customer_name: "fuchsia".into(),
                customer_id: 1,
                projects: vec![CobaltProjectConfig {
                    project_name: "test_project".into(),
                    project_id: 10,
                    metrics: vec![
                        MetricDefinition {
                            id: 100,
                            metric_name: "test_occurrence".into(),
                            metric_type: CobaltMetricType::Occurrence as i32,
                            metric_dimensions: vec![MetricDimension {
                                dimension: "dim1".into(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        },
                        MetricDefinition {
                            id: 101,
                            metric_name: "test_fire_histogram".into(),
                            metric_type: CobaltMetricType::IntegerHistogram as i32,
                            metric_dimensions: vec![
                                MetricDimension {
                                    dimension: "component".into(),
                                    ..Default::default()
                                },
                                MetricDimension {
                                    dimension: "reason".into(),
                                    ..Default::default()
                                },
                            ],
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn test_valid_config() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(1)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            }],
            fire_project_templates: vec![sampler_config::assembly::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![EventCode(2)], // +1 dimension for component = 2 dimensions
                    selectors: vec![],
                    upload_once: false,
                }],
            }],
            fire_component_configs: vec![],
        };

        assert!(validate(&bytes, &config).is_ok());
    }

    #[test]
    fn test_unknown_project() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(999),
                data_sets: vec![],
            }],
            ..Default::default()
        };

        let err = validate(&bytes, &config).unwrap_err();
        assert!(err.to_string().contains("project_id 999 not found"));
    }

    #[test]
    fn test_unknown_metric() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(999),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(1)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            }],
            ..Default::default()
        };

        let err = validate(&bytes, &config).unwrap_err();
        assert!(err.to_string().contains("Metric ID 999 in project 10 not found"));
    }

    #[test]
    fn test_metric_type_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Integer, // Cobalt has Occurrence!
                        event_codes: vec![EventCode(1)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            }],
            ..Default::default()
        };

        let err = validate(&bytes, &config).unwrap_err();
        assert!(err.to_string().contains("Metric type mismatch"));
    }

    #[test]
    fn test_dimension_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(1), EventCode(2)], // Cobalt expects 1!
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            }],
            ..Default::default()
        };

        let err = validate(&bytes, &config).unwrap_err();
        assert!(err.to_string().contains("Dimension count mismatch"));
    }

    #[test]
    fn test_fire_dimension_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let config = MergedSamplerConfig {
            fire_project_templates: vec![sampler_config::assembly::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![EventCode(1), EventCode(2)], // 2 + 1 component = 3 > 2 in Cobalt
                    selectors: vec![],
                    upload_once: false,
                }],
            }],
            ..Default::default()
        };

        let err = validate(&bytes, &config).unwrap_err();
        assert!(err.to_string().contains("Dimension mismatch for FIRE project 10"));
    }

    #[test]
    fn test_fewer_dimensions_allowed() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        // Project metric 100 has 1 dimension in Cobalt, but Sampler specifies 0 event codes.
        // FIRE template metric 101 has 2 dimensions in Cobalt, but Sampler specifies 0 event codes
        // (+1 for component = 1 dimension).
        // Both should be allowed since actual_dims <= expected_dims.
        let config = MergedSamplerConfig {
            project_configs: vec![SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            }],
            fire_project_templates: vec![sampler_config::assembly::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![], // 0 + 1 component = 1 <= 2 dimensions in Cobalt
                    selectors: vec![],
                    upload_once: false,
                }],
            }],
            fire_component_configs: vec![],
        };

        assert!(validate(&bytes, &config).is_ok());
    }
}
