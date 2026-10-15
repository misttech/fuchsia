// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, bail, format_err};
use argh::{ArgsInfo, FromArgs};
use cobalt_registry_proto::cobalt::metric_definition::MetricType as CobaltMetricType;
use cobalt_registry_proto::cobalt::{CobaltRegistry, MetricDefinition};
use fidl_fuchsia_diagnostics_common::Selector;
use prost::Message;
use sampler_config::assembly::{MergedSamplerConfig, MetricTemplate, ProjectTemplate};
use sampler_config::runtime::{DataSetConfig, MetricConfig, ProjectConfig as SamplerProjectConfig};
use sampler_config::{EventCode, MetricId, MetricType as SamplerMetricType, input};
use selectors::SelectorDisplayOptions;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

const FUCHSIA_CUSTOMER_ID: u32 = 1;

/// Index of the Cobalt dimension that the first event code of a FIRE metric template maps to.
/// The FIRE component ID is inserted as dimension 0.
const FIRE_EVENT_CODE_DIMENSION_OFFSET: usize = 1;

/// Diagnostics config command
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
pub struct GenerateConfigsCommand {
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
    pub cobalt_registry: PathBuf,

    /// path to which the result will be written.
    #[argh(option)]
    pub output: PathBuf,
}

pub fn main() -> Result<(), Error> {
    let args: GenerateConfigsCommand = argh::from_env();

    let mut project_configs = Vec::new();
    for project_config_path in args.project_config {
        let parsed = read_file(&project_config_path)?;
        project_configs.push((project_config_path, parsed));
    }
    let mut fire_project_templates = Vec::new();
    for project_template_path in args.fire_project_template {
        let parsed = read_file(&project_template_path)?;
        fire_project_templates.push((project_template_path, parsed));
    }
    let mut fire_component_configs = Vec::new();
    for component_config_path in args.fire_component_config {
        let parsed = read_file(&component_config_path)?;
        fire_component_configs.push(parsed);
    }

    validate_metric_ids_or_names(&project_configs, &fire_project_templates)?;

    let registry_bytes = std::fs::read(&args.cobalt_registry).with_context(|| {
        format!("Failed to read cobalt registry from {:?}", args.cobalt_registry)
    })?;
    let (project_configs, fire_project_templates) =
        resolve_names(&registry_bytes, project_configs, fire_project_templates)?;
    validate(&registry_bytes, &project_configs, &fire_project_templates)?;

    let config = MergedSamplerConfig {
        project_configs: project_configs.into_iter().map(|(_, c)| c).collect(),
        fire_project_templates: fire_project_templates.into_iter().map(|(_, t)| t).collect(),
        fire_component_configs,
    };

    write_file(args.output, config)?;

    Ok(())
}

fn format_errors(header: &str, errors: &[String]) -> anyhow::Error {
    let count = errors.len();
    let formatted_errors = errors
        .iter()
        .enumerate()
        .map(|(i, err)| format!("{}. {}", i + 1, err))
        .collect::<Vec<_>>()
        .join("\n\n");
    format_err!("{count} {header}:\n\n{formatted_errors}")
}

fn validate_metric_ids_or_names(
    project_configs: &[(PathBuf, input::ProjectConfig)],
    fire_project_templates: &[(PathBuf, input::ProjectTemplate)],
) -> Result<(), Error> {
    let mut errors = Vec::new();

    for (path, project) in project_configs {
        for dataset in &project.data_sets {
            for metric in &dataset.metrics {
                let selector_context = format_selectors_context(&metric.selectors);
                match (&metric.metric_id, &metric.metric_name) {
                    (None, None) => {
                        errors.push(format!(
                            "In {}: Metric must specify either metric_id or metric_name{}",
                            path.display(),
                            selector_context
                        ));
                    }
                    (Some(id), Some(name)) => {
                        errors.push(format!(
                            "In {}: Metric cannot specify both metric_id ({}) and metric_name ('{}'); specify either metric_id or metric_name, not both{}",
                            path.display(),
                            id,
                            name,
                            selector_context
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    for (path, template) in fire_project_templates {
        for metric in &template.metrics {
            let selector_context = format_template_selectors_context(&metric.selectors);
            match (&metric.metric_id, &metric.metric_name) {
                (None, None) => {
                    errors.push(format!(
                        "In {}: FIRE metric must specify either metric_id or metric_name{}",
                        path.display(),
                        selector_context
                    ));
                }
                (Some(id), Some(name)) => {
                    errors.push(format!(
                        "In {}: FIRE metric cannot specify both metric_id ({}) and metric_name ('{}'); specify either metric_id or metric_name, not both{}",
                        path.display(),
                        id,
                        name,
                        selector_context
                    ));
                }
                _ => {}
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format_errors("validation error(s) found in Sampler configs", &errors))
    }
}

struct CobaltProjectInfo<'a> {
    project_name: &'a str,
    metrics_by_id: HashMap<u32, &'a MetricDefinition>,
    metrics_by_name: HashMap<&'a str, &'a MetricDefinition>,
}

fn parse_cobalt_projects(
    registry: &CobaltRegistry,
) -> Result<HashMap<u32, CobaltProjectInfo<'_>>, Error> {
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
        let metrics_by_name: HashMap<&str, _> =
            project.metrics.iter().map(|m| (m.metric_name.as_str(), m)).collect();
        cobalt_projects.insert(
            project.project_id,
            CobaltProjectInfo {
                project_name: &project.project_name,
                metrics_by_id,
                metrics_by_name,
            },
        );
    }
    Ok(cobalt_projects)
}

/// Resolves the event code names in `event_codes` to their numeric codes.
///
/// `event_codes[i]` maps to Cobalt dimension `i + dimension_offset` of `cobalt_metric`.
/// `cobalt_metric` is `None` if the metric isn't defined in the Cobalt registry, in which case
/// only numeric event codes can be resolved.
///
/// Returns a description of each event code name that could not be resolved.
fn resolve_event_codes(
    event_codes: Vec<input::EventCodeSpec>,
    cobalt_metric: Option<&MetricDefinition>,
    dimension_offset: usize,
) -> Result<Vec<EventCode>, Vec<String>> {
    let mut resolved = Vec::with_capacity(event_codes.len());
    let mut errors = Vec::new();
    for (index, event_code) in event_codes.into_iter().enumerate() {
        let name = match event_code {
            input::EventCodeSpec::Code(code) => {
                resolved.push(code);
                continue;
            }
            input::EventCodeSpec::Name(name) => name,
        };
        let Some(cobalt_metric) = cobalt_metric else {
            errors.push(format!(
                "event_codes[{index}] ('{name}') cannot be resolved because the metric is not \
                 defined in the Cobalt registry"
            ));
            continue;
        };
        match resolve_event_code_name(&name, index, cobalt_metric, dimension_offset) {
            Ok(code) => resolved.push(code),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() { Ok(resolved) } else { Err(errors) }
}

/// Resolves the event code `name` at `event_codes[index]` using Cobalt dimension
/// `index + dimension_offset` of `cobalt_metric`.
fn resolve_event_code_name(
    name: &str,
    index: usize,
    cobalt_metric: &MetricDefinition,
    dimension_offset: usize,
) -> Result<EventCode, String> {
    let dimensions = &cobalt_metric.metric_dimensions;
    let Some(dimension) = dimensions.get(index + dimension_offset) else {
        let dimension_names: Vec<&str> = dimensions.iter().map(|d| d.dimension.as_str()).collect();
        let mut message = format!(
            "event_codes[{index}] ('{name}') has no corresponding Cobalt dimension: Cobalt defines \
             {} dimension(s) {dimension_names:?}",
            dimension_names.len()
        );
        let reserved_count = dimension_offset.min(dimension_names.len());
        if reserved_count > 0 {
            // Explain why FIRE event codes don't start at the first Cobalt dimension.
            let (reserved, available) = dimension_names.split_at(reserved_count);
            match available.first() {
                Some(first) => message.push_str(&format!(
                    ", and event_codes[0] maps to {first:?} because {reserved:?} is reserved for \
                     the FIRE component ID"
                )),
                None => message
                    .push_str(&format!(", and {reserved:?} is reserved for the FIRE component ID")),
            }
        }
        return Err(message);
    };
    if dimension.event_codes.is_empty() {
        return Err(format!(
            "event_codes[{index}] ('{name}'): Cobalt dimension '{}' does not define event code \
             names; use a numeric event code",
            dimension.dimension
        ));
    }
    let mut codes: Vec<u32> = dimension
        .event_codes
        .iter()
        .filter(|(_, code_name)| code_name.as_str() == name)
        .map(|(code, _)| *code)
        .collect();
    // `event_codes` is a `HashMap`, so sort the codes for a deterministic error message.
    codes.sort_unstable();
    match codes.as_slice() {
        [code] => Ok(EventCode(*code)),
        [] => Err(format!(
            "event_codes[{index}]: '{name}' is not an event code name in Cobalt dimension '{}'",
            dimension.dimension
        )),
        _ => Err(format!(
            "event_codes[{index}]: '{name}' matches multiple event codes {codes:?} in Cobalt \
             dimension '{}'",
            dimension.dimension
        )),
    }
}

fn resolve_names(
    registry_bytes: &[u8],
    project_configs: Vec<(PathBuf, input::ProjectConfig)>,
    fire_project_templates: Vec<(PathBuf, input::ProjectTemplate)>,
) -> Result<(Vec<(PathBuf, SamplerProjectConfig)>, Vec<(PathBuf, ProjectTemplate)>), Error> {
    let registry = CobaltRegistry::decode(registry_bytes)
        .context("Failed to decode CobaltRegistry protobuf")?;
    let cobalt_projects = parse_cobalt_projects(&registry)?;

    let mut errors = Vec::new();
    let mut resolved_projects = Vec::with_capacity(project_configs.len());

    for (path, project) in project_configs {
        let project_id = *project.project_id;
        let project_info = match cobalt_projects.get(&project_id) {
            Some(info) => info,
            None => {
                errors.push(format!(
                    "In {}: Sampler project_id {} not found in Cobalt registry",
                    path.display(),
                    project_id
                ));
                continue;
            }
        };

        let mut resolved_datasets = Vec::with_capacity(project.data_sets.len());
        for dataset in project.data_sets {
            let mut resolved_metrics = Vec::with_capacity(dataset.metrics.len());
            for metric in dataset.metrics {
                let (metric_id, cobalt_metric) = if let Some(id) = metric.metric_id {
                    (id, project_info.metrics_by_id.get(&*id).copied())
                } else if let Some(name) = metric.metric_name {
                    match project_info.metrics_by_name.get(name.as_str()) {
                        Some(cobalt_metric) => (MetricId(cobalt_metric.id), Some(*cobalt_metric)),
                        None => {
                            let selector_context = format_selectors_context(&metric.selectors);
                            errors.push(format!(
                                "In {}: Metric '{}' not found in Cobalt project {} ({}){}",
                                path.display(),
                                name,
                                project_id,
                                project_info.project_name,
                                selector_context
                            ));
                            continue;
                        }
                    }
                } else {
                    let selector_context = format_selectors_context(&metric.selectors);
                    errors.push(format!(
                        "In {}: Metric must specify either metric_id or metric_name{}",
                        path.display(),
                        selector_context
                    ));
                    continue;
                };

                // The event codes of a project metric map directly to the Cobalt dimensions.
                let event_codes = match resolve_event_codes(metric.event_codes, cobalt_metric, 0) {
                    Ok(event_codes) => event_codes,
                    Err(event_code_errors) => {
                        let selector_context = format_selectors_context(&metric.selectors);
                        errors.extend(event_code_errors.into_iter().map(|e| {
                            format!(
                                "In {}: Invalid event code for metric {} in project {} ({}): {}{}",
                                path.display(),
                                metric_id,
                                project_id,
                                project_info.project_name,
                                e,
                                selector_context
                            )
                        }));
                        continue;
                    }
                };

                resolved_metrics.push(MetricConfig {
                    selectors: metric.selectors,
                    metric_id,
                    metric_type: metric.metric_type,
                    event_codes,
                    upload_once: metric.upload_once,
                });
            }
            resolved_datasets.push(DataSetConfig {
                poll_rate_sec: dataset.poll_rate_sec,
                metrics: resolved_metrics,
            });
        }
        resolved_projects.push((
            path,
            SamplerProjectConfig { project_id: project.project_id, data_sets: resolved_datasets },
        ));
    }

    let mut resolved_templates = Vec::with_capacity(fire_project_templates.len());
    for (path, template) in fire_project_templates {
        let project_id = *template.project_id;
        let project_info = match cobalt_projects.get(&project_id) {
            Some(info) => info,
            None => {
                errors.push(format!(
                    "In {}: FIRE template project_id {} not found in Cobalt registry",
                    path.display(),
                    project_id
                ));
                continue;
            }
        };

        let mut resolved_metrics = Vec::with_capacity(template.metrics.len());
        for metric in template.metrics {
            let (metric_id, cobalt_metric) = if let Some(id) = metric.metric_id {
                (id, project_info.metrics_by_id.get(&*id).copied())
            } else if let Some(name) = metric.metric_name {
                match project_info.metrics_by_name.get(name.as_str()) {
                    Some(cobalt_metric) => (MetricId(cobalt_metric.id), Some(*cobalt_metric)),
                    None => {
                        let selector_context = format_template_selectors_context(&metric.selectors);
                        errors.push(format!(
                            "In {}: FIRE metric '{}' not found in Cobalt project {} ({}){}",
                            path.display(),
                            name,
                            project_id,
                            project_info.project_name,
                            selector_context
                        ));
                        continue;
                    }
                }
            } else {
                let selector_context = format_template_selectors_context(&metric.selectors);
                errors.push(format!(
                    "In {}: FIRE metric must specify either metric_id or metric_name{}",
                    path.display(),
                    selector_context
                ));
                continue;
            };

            let event_codes = match resolve_event_codes(
                metric.event_codes,
                cobalt_metric,
                FIRE_EVENT_CODE_DIMENSION_OFFSET,
            ) {
                Ok(event_codes) => event_codes,
                Err(event_code_errors) => {
                    let selector_context = format_template_selectors_context(&metric.selectors);
                    errors.extend(event_code_errors.into_iter().map(|e| {
                        format!(
                            "In {}: Invalid event code for FIRE metric {} in project {} ({}): {}{}",
                            path.display(),
                            metric_id,
                            project_id,
                            project_info.project_name,
                            e,
                            selector_context
                        )
                    }));
                    continue;
                }
            };

            resolved_metrics.push(MetricTemplate {
                selectors: metric.selectors,
                metric_id,
                metric_type: metric.metric_type,
                event_codes,
                upload_once: metric.upload_once,
            });
        }
        resolved_templates.push((
            path,
            ProjectTemplate {
                project_id: template.project_id,
                poll_rate_sec: template.poll_rate_sec,
                metrics: resolved_metrics,
            },
        ));
    }

    if errors.is_empty() {
        Ok((resolved_projects, resolved_templates))
    } else {
        Err(format_errors("validation error(s) found in Sampler configs", &errors))
    }
}

fn validate(
    registry_bytes: &[u8],
    project_configs: &[(PathBuf, SamplerProjectConfig)],
    fire_project_templates: &[(PathBuf, ProjectTemplate)],
) -> Result<(), Error> {
    let registry = CobaltRegistry::decode(registry_bytes)
        .context("Failed to decode CobaltRegistry protobuf")?;
    let cobalt_projects = parse_cobalt_projects(&registry)?;

    let mut errors = Vec::new();

    // Validate standard projects
    for (path, project) in project_configs {
        let project_id = *project.project_id;
        let (project_name, cobalt_metrics) = match cobalt_projects.get(&project_id) {
            Some(info) => (info.project_name, &info.metrics_by_id),
            None => {
                errors.push(format!(
                    "In {}: Sampler project_id {} not found in Cobalt registry",
                    path.display(),
                    project_id
                ));
                continue;
            }
        };

        for dataset in &project.data_sets {
            for metric in &dataset.metrics {
                let metric_id = *metric.metric_id;
                let selector_context = format_selectors_context(&metric.selectors);
                let cobalt_metric = match cobalt_metrics.get(&metric_id) {
                    Some(m) => m,
                    None => {
                        errors.push(format!(
                            "In {}: Metric ID {} not found in Cobalt project {} ({}){}",
                            path.display(),
                            metric_id,
                            project_id,
                            project_name,
                            selector_context
                        ));
                        continue;
                    }
                };

                if let Err(e) = verify_metric_type(metric.metric_type, cobalt_metric.metric_type) {
                    errors.push(format!(
                        "In {}: Metric type mismatch for metric {} ({}) in project {} ({}): {}{}",
                        path.display(),
                        metric_id,
                        cobalt_metric.metric_name,
                        project_id,
                        project_name,
                        e,
                        selector_context
                    ));
                }

                let expected_dim_names: Vec<&str> =
                    cobalt_metric.metric_dimensions.iter().map(|d| d.dimension.as_str()).collect();
                let actual_dims = metric.event_codes.len();
                if actual_dims > expected_dim_names.len() {
                    let actual_codes: Vec<u32> = metric.event_codes.iter().map(|c| c.0).collect();
                    errors.push(format!(
                        "In {}: Dimension count mismatch for metric {} ({}) in project {} ({}): \
                         Sampler config has {} event_codes ({:?}), but Cobalt defines {} dimension(s): {:?}{}",
                        path.display(),
                        metric_id,
                        cobalt_metric.metric_name,
                        project_id,
                        project_name,
                        actual_dims,
                        actual_codes,
                        expected_dim_names.len(),
                        expected_dim_names,
                        selector_context
                    ));
                }

                for e in check_event_codes(&metric.event_codes, cobalt_metric, 0) {
                    errors.push(format_invalid_event_code_error(
                        path,
                        "metric",
                        cobalt_metric,
                        project_id,
                        project_name,
                        &e,
                        &selector_context,
                    ));
                }
            }
        }
    }

    // Validate FIRE project templates
    for (path, template) in fire_project_templates {
        let project_id = *template.project_id;
        let (project_name, cobalt_metrics) = match cobalt_projects.get(&project_id) {
            Some(info) => (info.project_name, &info.metrics_by_id),
            None => {
                errors.push(format!(
                    "In {}: FIRE template project_id {} not found in Cobalt registry",
                    path.display(),
                    project_id
                ));
                continue;
            }
        };

        for metric in &template.metrics {
            let metric_id = *metric.metric_id;
            let selector_context = format_template_selectors_context(&metric.selectors);
            let cobalt_metric = match cobalt_metrics.get(&metric_id) {
                Some(m) => m,
                None => {
                    errors.push(format!(
                        "In {}: FIRE Metric ID {} not found in Cobalt project {} ({}){}",
                        path.display(),
                        metric_id,
                        project_id,
                        project_name,
                        selector_context
                    ));
                    continue;
                }
            };

            if let Err(e) = verify_metric_type(metric.metric_type, cobalt_metric.metric_type) {
                errors.push(format!(
                    "In {}: Metric type mismatch for FIRE metric {} ({}) in project {} ({}): {}{}",
                    path.display(),
                    metric_id,
                    cobalt_metric.metric_name,
                    project_id,
                    project_name,
                    e,
                    selector_context
                ));
            }

            // In FIRE templates, component ID is injected as dimension 0, so event_codes.len() + 1
            let expected_dim_names: Vec<&str> =
                cobalt_metric.metric_dimensions.iter().map(|d| d.dimension.as_str()).collect();
            let actual_dims = metric.event_codes.len() + 1;
            if actual_dims > expected_dim_names.len() {
                let actual_codes: Vec<u32> = metric.event_codes.iter().map(|c| c.0).collect();
                errors.push(format!(
                    "In {}: Dimension count mismatch for FIRE metric {} ({}) in project {} ({}): \
                     Sampler has {} event_codes ({:?}) + 1 for component = {actual_dims}, \
                     but Cobalt defines {} dimension(s): {:?}{}",
                    path.display(),
                    metric_id,
                    cobalt_metric.metric_name,
                    project_id,
                    project_name,
                    metric.event_codes.len(),
                    actual_codes,
                    expected_dim_names.len(),
                    expected_dim_names,
                    selector_context
                ));
            }

            for e in check_event_codes(
                &metric.event_codes,
                cobalt_metric,
                FIRE_EVENT_CODE_DIMENSION_OFFSET,
            ) {
                errors.push(format_invalid_event_code_error(
                    path,
                    "FIRE metric",
                    cobalt_metric,
                    project_id,
                    project_name,
                    &e,
                    &selector_context,
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format_errors("validation error(s) found in Sampler configs", &errors))
    }
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

/// Checks that each event code is valid for the Cobalt dimension it maps to.
///
/// `event_codes[i]` maps to Cobalt dimension `i + dimension_offset` of `cobalt_metric`. A code is
/// valid if the dimension defines it, or if it doesn't exceed the dimension's `max_event_code`.
/// Event codes without a corresponding dimension are skipped, since the dimension count is checked
/// separately.
///
/// Returns a description of each invalid event code.
fn check_event_codes(
    event_codes: &[EventCode],
    cobalt_metric: &MetricDefinition,
    dimension_offset: usize,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, code) in event_codes.iter().enumerate() {
        let Some(dimension) = cobalt_metric.metric_dimensions.get(index + dimension_offset) else {
            continue;
        };
        // A `max_event_code` of 0 means it isn't set, so only the defined codes are valid.
        let max_event_code = dimension.max_event_code;
        if dimension.event_codes.contains_key(&code.0)
            || (max_event_code > 0 && code.0 <= max_event_code)
        {
            continue;
        }
        let mut error = format!(
            "event_codes[{index}] = {code} is not defined in Cobalt dimension '{}'",
            dimension.dimension
        );
        if max_event_code > 0 {
            error.push_str(&format!(" and exceeds its max_event_code ({max_event_code})"));
        }
        errors.push(error);
    }
    errors
}

/// Formats `error`, which describes an invalid event code for `cobalt_metric`, with the location
/// of the Sampler metric. `metric_kind` describes the Sampler metric, e.g. "FIRE metric".
fn format_invalid_event_code_error(
    path: &Path,
    metric_kind: &str,
    cobalt_metric: &MetricDefinition,
    project_id: u32,
    project_name: &str,
    error: &str,
    selector_context: &str,
) -> String {
    let path = path.display();
    let MetricDefinition { id: metric_id, metric_name, .. } = cobalt_metric;
    format!(
        "In {path}: Invalid event code for {metric_kind} {metric_id} ({metric_name}) in project \
         {project_id} ({project_name}): {error}{selector_context}"
    )
}

fn format_selectors_context(selectors: &[Selector]) -> String {
    let strs: Vec<_> = selectors
        .iter()
        .filter_map(|s| {
            selectors::selector_to_string(s, SelectorDisplayOptions::never_wrap_in_quotes()).ok()
        })
        .collect();
    if strs.is_empty() { String::new() } else { format!("\n  Selector: {}", strs.join(", ")) }
}

fn format_template_selectors_context(selectors: &[String]) -> String {
    if selectors.is_empty() {
        String::new()
    } else {
        format!("\n  Selector: {}", selectors.join(", "))
    }
}

fn read_file<T: DeserializeOwned>(path: impl AsRef<Path>) -> anyhow::Result<T> {
    let file =
        File::open(path.as_ref()).with_context(|| format!("Failed to open {:?}", path.as_ref()))?;
    let mut reader = BufReader::new(file);
    let result: T = serde_json5::from_reader(&mut reader)
        .with_context(|| format!("Failed to parse JSON5 from {:?}", path.as_ref()))?;
    Ok(result)
}

fn write_file<T: Serialize>(path: PathBuf, value: T) -> anyhow::Result<()> {
    let file =
        File::create(&path).with_context(|| format!("Failed to create output file {:?}", path))?;
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
    use sampler_config::ProjectId;

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
                                max_event_code: 10,
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
                                    event_codes: [(1, "Crash".into()), (2, "Timeout".into())]
                                        .into_iter()
                                        .collect(),
                                    ..Default::default()
                                },
                            ],
                            ..Default::default()
                        },
                        MetricDefinition {
                            id: 102,
                            metric_name: "test_named_occurrence".into(),
                            metric_type: CobaltMetricType::Occurrence as i32,
                            metric_dimensions: vec![
                                MetricDimension {
                                    dimension: "status".into(),
                                    event_codes: [(0, "Ok".into()), (1, "Failed".into())]
                                        .into_iter()
                                        .collect(),
                                    ..Default::default()
                                },
                                MetricDimension {
                                    dimension: "count".into(),
                                    max_event_code: 10,
                                    ..Default::default()
                                },
                                MetricDimension {
                                    dimension: "duplicated".into(),
                                    event_codes: [(1, "Same".into()), (2, "Same".into())]
                                        .into_iter()
                                        .collect(),
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

        let project_configs = vec![(
            PathBuf::from("test/project.json5"),
            SamplerProjectConfig {
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
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/fire.json5"),
            ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![EventCode(2)],
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        assert!(validate(&bytes, &project_configs, &fire_templates).is_ok());
    }

    #[test]
    fn test_unknown_project() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_project.json5"),
            SamplerProjectConfig { project_id: ProjectId(999), data_sets: vec![] },
        )];

        let err = validate(&bytes, &project_configs, &[]).unwrap_err();
        assert!(
            err.to_string().contains("In test/bad_project.json5: Sampler project_id 999 not found")
        );
    }

    #[test]
    fn test_unknown_metric() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let selector = selectors::parse_verbose("core/foo:root:bar").unwrap();
        let project_configs = vec![(
            PathBuf::from("test/bad_metric.json5"),
            SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(999),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(1)],
                        selectors: vec![selector],
                        upload_once: false,
                    }],
                }],
            },
        )];

        let err = validate(&bytes, &project_configs, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(
            "In test/bad_metric.json5: Metric ID 999 not found in Cobalt project 10 (test_project)"
        ));
        assert!(msg.contains("Selector: core/foo:root:bar"));
    }

    #[test]
    fn test_metric_type_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_type.json5"),
            SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Integer,
                        event_codes: vec![EventCode(1)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];

        let err = validate(&bytes, &project_configs, &[]).unwrap_err();
        assert!(err.to_string().contains("In test/bad_type.json5: Metric type mismatch for metric 100 (test_occurrence) in project 10 (test_project): Sampler specified Integer, Cobalt defines Occurrence"));
    }

    #[test]
    fn test_dimension_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_dims.json5"),
            SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(100),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(1), EventCode(2)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];

        let err = validate(&bytes, &project_configs, &[]).unwrap_err();
        assert!(err.to_string().contains("In test/bad_dims.json5: Dimension count mismatch for metric 100 (test_occurrence) in project 10 (test_project): Sampler config has 2 event_codes ([1, 2]), but Cobalt defines 1 dimension(s): [\"dim1\"]"));
    }

    #[test]
    fn test_fire_dimension_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let fire_templates = vec![(
            PathBuf::from("test/bad_fire.json5"),
            ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![EventCode(1), EventCode(2)], // 2 + 1 component = 3 > 2 in Cobalt
                    selectors: vec!["core/fire:root:val".to_string()],
                    upload_once: false,
                }],
            },
        )];

        let err = validate(&bytes, &[], &fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("In test/bad_fire.json5: Dimension count mismatch for FIRE metric 101 (test_fire_histogram) in project 10 (test_project): Sampler has 2 event_codes ([1, 2]) + 1 for component = 3, but Cobalt defines 2 dimension(s): [\"component\", \"reason\"]"));
        assert!(msg.contains("Selector: core/fire:root:val"));
    }

    #[test]
    fn test_fewer_dimensions_allowed() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        // Project metric 100 has 1 dimension in Cobalt, but Sampler specifies 0 event codes.
        // FIRE template metric 101 has 2 dimensions in Cobalt, but Sampler specifies 0 event codes
        // (+1 for component = 1 dimension).
        // Both should be allowed since actual_dims <= expected_dims.
        let project_configs = vec![(
            PathBuf::from("test/fewer_dims.json5"),
            SamplerProjectConfig {
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
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/fewer_fire_dims.json5"),
            ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![], // 0 + 1 component = 1 <= 2 dimensions in Cobalt
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        assert!(validate(&bytes, &project_configs, &fire_templates).is_ok());
    }

    #[test]
    fn test_multiple_errors_accumulated() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![
            (
                PathBuf::from("test/bad_project1.json5"),
                SamplerProjectConfig { project_id: ProjectId(999), data_sets: vec![] },
            ),
            (
                PathBuf::from("test/bad_project2.json5"),
                SamplerProjectConfig {
                    project_id: ProjectId(10),
                    data_sets: vec![DataSetConfig {
                        poll_rate_sec: 60,
                        metrics: vec![
                            MetricConfig {
                                metric_id: MetricId(100),
                                metric_type: SamplerMetricType::Integer, // mismatch
                                event_codes: vec![EventCode(1)],
                                selectors: vec![],
                                upload_once: false,
                            },
                            MetricConfig {
                                metric_id: MetricId(999), // unknown metric
                                metric_type: SamplerMetricType::Occurrence,
                                event_codes: vec![],
                                selectors: vec![],
                                upload_once: false,
                            },
                        ],
                    }],
                },
            ),
        ];

        let err = validate(&bytes, &project_configs, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3 validation error(s) found in Sampler configs:"));
        assert!(msg.contains("1. In test/bad_project1.json5: Sampler project_id 999 not found"));
        assert!(msg.contains("2. In test/bad_project2.json5: Metric type mismatch"));
        assert!(msg.contains("3. In test/bad_project2.json5: Metric ID 999 not found"));
    }

    #[test]
    fn test_valid_config_with_metric_name() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/project.json5"),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id: None,
                        metric_name: Some("test_occurrence".into()),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![input::EventCodeSpec::Code(EventCode(1))],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/fire.json5"),
            input::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![input::MetricTemplate {
                    metric_id: None,
                    metric_name: Some("test_fire_histogram".into()),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![input::EventCodeSpec::Code(EventCode(2))],
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        assert!(validate_metric_ids_or_names(&project_configs, &fire_templates).is_ok());
        let (resolved_projects, resolved_fire_templates) =
            resolve_names(&bytes, project_configs, fire_templates).expect("resolve names");
        assert_eq!(resolved_projects[0].1.data_sets[0].metrics[0].metric_id, MetricId(100));
        assert_eq!(resolved_fire_templates[0].1.metrics[0].metric_id, MetricId(101));

        assert!(validate(&bytes, &resolved_projects, &resolved_fire_templates).is_ok());
    }

    #[test]
    fn test_unknown_metric_name() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_name.json5"),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id: None,
                        metric_name: Some("nonexistent_metric".into()),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/bad_fire_name.json5"),
            input::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![input::MetricTemplate {
                    metric_id: None,
                    metric_name: Some("nonexistent_fire_metric".into()),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![],
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        let err = resolve_names(&bytes, project_configs, fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(
            "In test/bad_name.json5: Metric 'nonexistent_metric' not found in Cobalt project 10 (test_project)"
        ));
        assert!(msg.contains(
            "In test/bad_fire_name.json5: FIRE metric 'nonexistent_fire_metric' not found in Cobalt project 10 (test_project)"
        ));
    }

    #[test]
    fn test_missing_both_metric_id_and_name() {
        let project_configs = vec![(
            PathBuf::from("test/missing_both.json5"),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id: None,
                        metric_name: None,
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/missing_both_fire.json5"),
            input::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![input::MetricTemplate {
                    metric_id: None,
                    metric_name: None,
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![],
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        let err = validate_metric_ids_or_names(&project_configs, &fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(
            "In test/missing_both.json5: Metric must specify either metric_id or metric_name"
        ));
        assert!(msg.contains(
            "In test/missing_both_fire.json5: FIRE metric must specify either metric_id or metric_name"
        ));
    }

    #[test]
    fn test_specifying_both_metric_id_and_name() {
        let project_configs = vec![(
            PathBuf::from("test/both.json5"),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id: Some(MetricId(100)),
                        metric_name: Some("test_occurrence".into()),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![input::EventCodeSpec::Code(EventCode(1))],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];
        let fire_templates = vec![(
            PathBuf::from("test/both_fire.json5"),
            input::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![input::MetricTemplate {
                    metric_id: Some(MetricId(101)),
                    metric_name: Some("test_fire_histogram".into()),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![input::EventCodeSpec::Code(EventCode(2))],
                    selectors: vec![],
                    upload_once: false,
                }],
            },
        )];

        let err = validate_metric_ids_or_names(&project_configs, &fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(
            "In test/both.json5: Metric cannot specify both metric_id (100) and metric_name ('test_occurrence'); specify either metric_id or metric_name, not both"
        ));
        assert!(msg.contains(
            "In test/both_fire.json5: FIRE metric cannot specify both metric_id (101) and metric_name ('test_fire_histogram'); specify either metric_id or metric_name, not both"
        ));
    }

    #[test]
    fn test_metric_name_type_and_dimension_mismatch() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_resolved_metric.json5"),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id: None,
                        metric_name: Some("test_occurrence".into()),
                        metric_type: SamplerMetricType::Integer, // mismatch: Cobalt defines Occurrence
                        // mismatch: Cobalt defines 1 dim
                        event_codes: vec![
                            input::EventCodeSpec::Code(EventCode(1)),
                            input::EventCodeSpec::Code(EventCode(2)),
                        ],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];

        assert!(validate_metric_ids_or_names(&project_configs, &[]).is_ok());
        let (resolved_projects, resolved_fire_templates) =
            resolve_names(&bytes, project_configs, vec![]).expect("resolve names");
        let err = validate(&bytes, &resolved_projects, &resolved_fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(
            "In test/bad_resolved_metric.json5: Metric type mismatch for metric 100 (test_occurrence) in project 10 (test_project): Sampler specified Integer, Cobalt defines Occurrence"
        ));
        assert!(msg.contains(
            "In test/bad_resolved_metric.json5: Dimension count mismatch for metric 100 (test_occurrence) in project 10 (test_project): Sampler config has 2 event_codes ([1, 2]), but Cobalt defines 1 dimension(s): [\"dim1\"]"
        ));
    }

    fn make_input_project_config(
        path: &str,
        metric_id: Option<MetricId>,
        metric_name: Option<&str>,
        event_codes: Vec<input::EventCodeSpec>,
    ) -> (PathBuf, input::ProjectConfig) {
        (
            PathBuf::from(path),
            input::ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![input::DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![input::MetricConfig {
                        metric_id,
                        metric_name: metric_name.map(Into::into),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes,
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )
    }

    fn make_input_fire_template(
        path: &str,
        event_codes: Vec<input::EventCodeSpec>,
    ) -> (PathBuf, input::ProjectTemplate) {
        (
            PathBuf::from(path),
            input::ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![input::MetricTemplate {
                    metric_id: None,
                    metric_name: Some("test_fire_histogram".into()),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes,
                    selectors: vec!["{MONIKER}:root:val".into()],
                    upload_once: false,
                }],
            },
        )
    }

    fn name(name: &str) -> input::EventCodeSpec {
        input::EventCodeSpec::Name(name.into())
    }

    #[test]
    fn test_resolve_event_code_names() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![
            make_input_project_config(
                "test/by_name.json5",
                None,
                Some("test_named_occurrence"),
                vec![name("Failed"), input::EventCodeSpec::Code(EventCode(3))],
            ),
            make_input_project_config(
                "test/by_id.json5",
                Some(MetricId(102)),
                None,
                vec![name("Ok")],
            ),
        ];
        // The FIRE component ID is dimension 0, so event_codes[0] maps to the "reason" dimension.
        let fire_templates =
            vec![make_input_fire_template("test/fire.json5", vec![name("Timeout")])];

        let (resolved_projects, resolved_fire_templates) =
            resolve_names(&bytes, project_configs, fire_templates).expect("resolve names");
        assert_eq!(
            resolved_projects[0].1.data_sets[0].metrics[0].event_codes,
            vec![EventCode(1), EventCode(3)]
        );
        assert_eq!(resolved_projects[1].1.data_sets[0].metrics[0].event_codes, vec![EventCode(0)]);
        assert_eq!(resolved_fire_templates[0].1.metrics[0].event_codes, vec![EventCode(2)]);

        assert!(validate(&bytes, &resolved_projects, &resolved_fire_templates).is_ok());
    }

    #[test]
    fn test_fire_event_code_name_with_no_cobalt_dimensions() {
        let metric = MetricDefinition { metric_name: "no_dims".into(), ..Default::default() };
        let err = resolve_event_code_name("Crash", 0, &metric, FIRE_EVENT_CODE_DIMENSION_OFFSET)
            .unwrap_err();
        // No dimension is reserved for the FIRE component ID, so the message shouldn't mention one.
        assert_eq!(
            err,
            "event_codes[0] ('Crash') has no corresponding Cobalt dimension: Cobalt defines 0 \
             dimension(s) []"
        );
    }

    #[test]
    fn test_unresolvable_event_code_names() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![
            make_input_project_config(
                "test/bad_names.json5",
                None,
                Some("test_named_occurrence"),
                vec![name("Unknown"), name("X"), name("Same"), name("Extra")],
            ),
            make_input_project_config(
                "test/unknown_metric.json5",
                Some(MetricId(999)),
                None,
                vec![name("Ok")],
            ),
        ];
        let fire_templates = vec![make_input_fire_template(
            "test/bad_fire.json5",
            vec![name("Crash"), name("Extra")],
        )];

        let err = resolve_names(&bytes, project_configs, fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("6 validation error(s) found in Sampler configs:"), "{msg}");
        let prefix = "In test/bad_names.json5: Invalid event code for metric 102 in project 10 \
                      (test_project):";
        assert!(msg.contains(&format!(
            "{prefix} event_codes[0]: 'Unknown' is not an event code name in Cobalt dimension \
             'status'"
        )));
        assert!(msg.contains(&format!(
            "{prefix} event_codes[1] ('X'): Cobalt dimension 'count' does not define event code \
             names; use a numeric event code"
        )));
        assert!(msg.contains(&format!(
            "{prefix} event_codes[2]: 'Same' matches multiple event codes [1, 2] in Cobalt \
             dimension 'duplicated'"
        )));
        assert!(msg.contains(&format!(
            "{prefix} event_codes[3] ('Extra') has no corresponding Cobalt dimension: Cobalt \
             defines 3 dimension(s) [\"status\", \"count\", \"duplicated\"]"
        )));
        assert!(msg.contains(
            "In test/unknown_metric.json5: Invalid event code for metric 999 in project 10 \
             (test_project): event_codes[0] ('Ok') cannot be resolved because the metric is not \
             defined in the Cobalt registry"
        ));
        assert!(msg.contains(
            "In test/bad_fire.json5: Invalid event code for FIRE metric 101 in project 10 \
             (test_project): event_codes[1] ('Extra') has no corresponding Cobalt dimension: \
             Cobalt defines 2 dimension(s) [\"component\", \"reason\"], and event_codes[0] maps \
             to \"reason\" because [\"component\"] is reserved for the FIRE component ID\n  \
             Selector: {MONIKER}:root:val"
        ));
    }

    #[test]
    fn test_valid_numeric_event_codes() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        // Codes defined by the dimension, and codes up to the dimension's max_event_code, are
        // valid.
        let project_configs = vec![(
            PathBuf::from("test/valid_codes.json5"),
            SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(102),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(0), EventCode(10), EventCode(2)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];

        assert!(validate(&bytes, &project_configs, &[]).is_ok());
    }

    #[test]
    fn test_invalid_numeric_event_codes() {
        let registry = make_test_registry();
        let bytes = registry.encode_to_vec();

        let project_configs = vec![(
            PathBuf::from("test/bad_codes.json5"),
            SamplerProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        metric_id: MetricId(102),
                        metric_type: SamplerMetricType::Occurrence,
                        event_codes: vec![EventCode(5), EventCode(11), EventCode(1)],
                        selectors: vec![],
                        upload_once: false,
                    }],
                }],
            },
        )];
        // The FIRE component ID is dimension 0, so event_codes[0] maps to the "reason" dimension.
        let fire_templates = vec![(
            PathBuf::from("test/bad_fire_codes.json5"),
            ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    metric_id: MetricId(101),
                    metric_type: SamplerMetricType::IntHistogram,
                    event_codes: vec![EventCode(3)],
                    selectors: vec!["core/fire:root:val".to_string()],
                    upload_once: false,
                }],
            },
        )];

        let err = validate(&bytes, &project_configs, &fire_templates).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("3 validation error(s) found in Sampler configs:"), "{msg}");
        let prefix = "In test/bad_codes.json5: Invalid event code for metric 102 \
                      (test_named_occurrence) in project 10 (test_project):";
        assert!(msg.contains(&format!(
            "{prefix} event_codes[0] = 5 is not defined in Cobalt dimension 'status'"
        )));
        assert!(msg.contains(&format!(
            "{prefix} event_codes[1] = 11 is not defined in Cobalt dimension 'count' and exceeds \
             its max_event_code (10)"
        )));
        assert!(msg.contains(
            "In test/bad_fire_codes.json5: Invalid event code for FIRE metric 101 \
             (test_fire_histogram) in project 10 (test_project): event_codes[0] = 3 is not \
             defined in Cobalt dimension 'reason'\n  Selector: core/fire:root:val"
        ));
    }
}
