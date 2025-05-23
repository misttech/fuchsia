// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Context, Error, Result};
use cm_fidl_analyzer::component_model::{AnalyzerModelError, ComponentModelForAnalyzer};
use cm_fidl_analyzer::route::VerifyRouteResult;
use cm_rust::{CapabilityDecl, CapabilityTypeName, ComponentDecl, ExposeDecl, OfferDecl, UseDecl};
use cm_types::{Name, Path, RelativePath};
use futures::FutureExt;
use moniker::Moniker;
use routing::component_instance::ComponentInstanceInterface;
use routing::mapper::RouteSegment;
use scrutiny_collection::core::{Component, ComponentSource, Components};
use scrutiny_collection::model::DataModel;
use scrutiny_collection::v2_component_model::V2ComponentModel;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::read_to_string;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error as ThisError;

const MISSING_TARGET_INSTANCE: &str = "Target instance is missing from component model";
const GATHER_FAILED: &str = "Target instance failed to gather routes_to_skip and routes_to_verify";
const ROUTE_LISTS_INCOMPLETE: &str = "Component route skip list + verify list incomplete";
const ROUTE_LISTS_OVERLAP: &str = "Component route skip list + verify list contains duplicates";
const MATCH_ONE_FAILED: &str = "Failed to match exactly one item";
const USE_SPEC_NAME: &str = "use spec";
const USE_DECL_NAME: &str = "use declaration";

#[derive(Deserialize, Serialize)]
pub struct RouteSourcesRequest {
    // The input path to a json5 file containing `RouteSourcesConfig`.
    pub input: String,
}

/// Content of a request to a `RouteSourcesController`, containing an exhuastive
/// list of routes to zero or more component instances.
#[derive(Deserialize, Serialize)]
pub struct RouteSourcesConfig {
    pub component_routes: Vec<RouteSourcesSpec>,
}

/// Specification of all routes to a component instance in the component tree.
/// Each route must be listed, either to be verified or skipped by the verifier.
#[derive(Deserialize, Serialize)]
pub struct RouteSourcesSpec {
    /// TODO(https://fxbug.dev/42053778): rename to `target_moniker`.
    /// `Moniker` of the component instance whose routes are to be verified.
    pub target_node_path: Moniker,
    /// If true, verification of the component instance will be skipped if the
    /// component instance does not exist in the component tree.
    /// Allows soft transitions of adding/removing/renaming components.
    #[serde(default)]
    pub skip_if_target_node_missing: bool,
    /// Routes that are expected to be present, but do not require verification.
    pub routes_to_skip: Vec<UseSpec>,
    /// Route specification and route source matching information for routes
    /// that are to be verified.
    pub routes_to_verify: Vec<RouteMatch>,
}

/// Input query type for matching routes by target. Usually, either a `path` or
/// a `name` is specified.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UseSpec {
    /// Route capability types.
    #[serde(rename = "use_type")]
    pub type_name: CapabilityTypeName,
    /// Target capability path the match, if any.
    #[serde(rename = "use_path")]
    pub path: Option<Path>,
    /// Target capability name, if any.
    #[serde(rename = "use_name")]
    pub name: Option<Name>,
}

/// Match a `UseDecl` to a `UseSpec` when types match and spec'd `name` and/or
/// `path` matches.
impl Matches<UseDecl> for UseSpec {
    const NAME: &'static str = USE_SPEC_NAME;
    const OTHER_NAME: &'static str = USE_DECL_NAME;

    fn matches(&self, other: &UseDecl) -> Result<bool> {
        Ok(self.type_name == other.into()
            && (self.path.is_none() || self.path.as_ref() == other.path())
            && (self.name.is_none() || self.name.as_ref() == other.name()))
    }
}

/// Input query type for matching routes by target and source.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RouteMatch {
    /// Route information to match an entry in the component instance `uses`
    /// array.
    #[serde(flatten)]
    target: UseSpec,
    /// Route source information to match the capability declaration at the
    /// route's source.
    #[serde(flatten)]
    source: SourceSpec,
}

/// Input query type for matching a route source.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SourceSpec {
    /// `Moniker` prefix expected at the source instance.
    /// TODO(https://fxbug.dev/42053778): Rename serde key to to `source_moniker`.
    #[serde(rename = "source_node_path")]
    moniker: Moniker,
    /// Capability declaration expected at the source instance.
    #[serde(flatten)]
    capability: SourceDeclSpec,
}

/// Input query type for matching a capability declaration. Usually, either a
// `path` or a `name` is specified.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SourceDeclSpec {
    /// Path designated in capability declaration, if any.
    #[serde(rename = "source_path_prefix")]
    pub path_prefix: Option<Path>,
    /// Name designated in capability declaration, if any.
    #[serde(rename = "source_name")]
    pub name: Option<Name>,
}

/// Match a `CapabilityDecl` to a `SourceDeclSpec`. Only the `name` is matched because matching
/// `path_prefix` requires complete capability route.
impl Matches<CapabilityDecl> for SourceDeclSpec {
    const NAME: &'static str = "source declaration spec";
    const OTHER_NAME: &'static str = "capability declaration";

    fn matches(&self, other: &CapabilityDecl) -> Result<bool> {
        Ok(match &self.name {
            Some(name) => name == other.name(),
            None => true,
        })
    }
}

/// Match a complete route against to a `SourceDeclSpec` by following `subdir`
/// designations along route and delegating to `Matches<CapabilityDecl>`
/// implementation.
impl Matches<Vec<RouteSegment>> for SourceDeclSpec {
    const NAME: &'static str = "source declaration spec";
    const OTHER_NAME: &'static str = "route";

    fn matches(&self, other: &Vec<RouteSegment>) -> Result<bool> {
        // Spec cannot match empty route.
        let decl = other.last();
        if decl.is_none() {
            return Ok(false);
        }
        let decl = decl.unwrap();

        // Accumulate `subdir` onto `decl.capability.source_path`, where applicable, and delegate
        // to `self.matches(&CapabilityDecl)`.
        match decl {
            RouteSegment::DeclareBy { capability, .. } => match &capability {
                CapabilityDecl::Directory(decl) => {
                    if self.path_prefix.is_none() || decl.source_path.is_none() {
                        return Ok(false);
                    }
                    let path_prefix = self.path_prefix.as_ref().unwrap();
                    let source_path = &decl.source_path.as_ref().unwrap();
                    let subdirs = get_subdirs(other);
                    let source_path_str = subdirs.iter().fold(source_path.to_path_buf(), |path_buf, next| {
                            let mut next_buf = path_buf;
                            next_buf.push(next.to_path_buf());
                            next_buf
                        }).to_str().ok_or_else(|| anyhow!("Failed to format PathBuf as string; components; {:?} appended with {:?}", decl.source_path, subdirs))?.to_string();
                    let source_path = Path::from_str(&source_path_str).with_context(|| {
                        anyhow!("Failed to parse string into Path: {}", source_path_str)
                    })?;

                    Ok(match_path_prefix(path_prefix, &source_path))
                }
                _ => Ok(false),
            },
            _ => Ok(false),
        }
    }
}

fn get_subdirs(route: &Vec<RouteSegment>) -> Vec<RelativePath> {
    let mut subdir = vec![];
    for segment in route.iter() {
        match segment {
            RouteSegment::UseBy { capability, .. } => match capability {
                UseDecl::Directory(decl) => {
                    if !decl.subdir.is_dot() {
                        subdir.push(decl.subdir.clone());
                    }
                }
                _ => {}
            },
            RouteSegment::OfferBy { capability, .. } => match capability {
                OfferDecl::Directory(decl) => {
                    if !decl.subdir.is_dot() {
                        subdir.push(decl.subdir.clone());
                    }
                }
                _ => {}
            },
            RouteSegment::ExposeBy { capability, .. } => match capability {
                ExposeDecl::Directory(decl) => {
                    if !decl.subdir.is_dot() {
                        subdir.push(decl.subdir.clone());
                    }
                }
                _ => {}
            },
            RouteSegment::DeclareBy { capability, .. } => match capability {
                CapabilityDecl::Storage(decl) => {
                    if !decl.subdir.is_dot() {
                        subdir.push(decl.subdir.clone());
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // Subdirs collected target->source, but are applied source->target.
    subdir.reverse();

    subdir
}

fn match_path_prefix(prefix: &Path, path: &Path) -> bool {
    let prefix = prefix.split();
    let path = path.split();
    if prefix.len() > path.len() {
        return false;
    }
    for (i, expected_segment) in prefix.iter().enumerate() {
        let actual_segment = &path[i];
        if expected_segment != actual_segment {
            return false;
        }
    }

    true
}

/// Output type: Wrapper for full set of results and dependencies.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct VerifyRouteSourcesResults {
    pub deps: HashSet<PathBuf>,
    pub results: HashMap<String, Vec<VerifyRouteSourcesResult>>,
}

/// Output type: The result of matching a route use+source specification against
/// routes in the component tree.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct VerifyRouteSourcesResult {
    pub query: RouteMatch,
    pub result: Result<Source, RouteSourceError>,
}

/// Output type: The source of a capability route.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct Source {
    /// The `Moniker` of the declaring component instance.
    moniker: Moniker,
    /// The capability declaration.
    capability: CapabilityDecl,
}

/// Output type: Structured errors for matching a route+use specification
/// against routes in the component tree.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum RouteSourceError {
    AnalyzerModelError(AnalyzerModelError),
    RouteSegmentWithoutComponent(RouteSegment),
    RouteSegmentAbsMonikerNotFoundInTree(RouteSegment),
    ComponentInstanceLookupByUrlFailed(String),
    MultipleComponentsWithSameUrl(Vec<Component>),
    RouteSegmentComponentFromUntrustedSource(RouteSegment, ComponentSource),
    RouteMismatch(Source),
    MissingSourceCapability(RouteSegment),
    InvalidUrl(String),
}

/// Intermediate value for use declarations that match a use+source spec.
struct Binding<'a> {
    route_match: &'a RouteMatch,
    use_decl: &'a UseDecl,
}

#[derive(Debug, ThisError)]
enum MatchOneError {
    #[error("{0:?}")]
    NoMatch(Error),
    #[error("{0:?}")]
    MultipleMatches(Error),
}

/// Trait for specifying a match between implementer and `Other` type values,
/// and matching exactly one `Other` from a vector. Matching functions may
/// return an error when input processing operations fail unexpectedly.
trait Matches<Other>: fmt::Debug + Sized + Serialize
where
    Other: fmt::Debug,
{
    const NAME: &'static str;
    const OTHER_NAME: &'static str;

    fn matches(&self, other: &Other) -> Result<bool>;

    fn match_one<'a>(&self, other: &'a Vec<Other>) -> Result<&'a Other, MatchOneError> {
        // Ignore match errors: Only interested in number of positive matches.
        let matches: Vec<&'a Other> = other
            .iter()
            .filter_map(|other| match self.matches(other) {
                Ok(true) => Some(other),
                _ => None,
            })
            .collect();

        if matches.len() == 0 {
            return Err(MatchOneError::NoMatch(anyhow!(
                "{}; no {}: {:#?} in {} list: {:#?}",
                MATCH_ONE_FAILED,
                Self::NAME,
                self,
                Self::OTHER_NAME,
                other
            )));
        } else if matches.len() > 1 {
            return Err(MatchOneError::MultipleMatches(anyhow!(
                "Multiple instances of {} in {:#?} matches {} {:#?}; matches: {:#?}",
                Self::OTHER_NAME,
                other,
                Self::NAME,
                self,
                matches
            )));
        }

        Ok(matches[0])
    }
}

/// Helper for attempting to embed JSON in error messages.
fn json_or_unformatted<S>(value: &S, type_name: &str) -> String
where
    S: Serialize,
{
    serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("<unformatted {}>", type_name))
}

/// Attempt to gather exactly one match for all `routes_to_skip` and all
/// `routes_to_verify` in `component_routes`.
fn gather_routes<'a>(
    component_routes: &'a RouteSourcesSpec,
    component_decl: &'a ComponentDecl,
    moniker: &'a Moniker,
) -> Result<(Vec<&'a UseDecl>, Vec<Binding<'a>>)> {
    let uses = &component_decl.uses;
    let routes_to_skip = component_routes
        .routes_to_skip
        .iter()
        .filter_map(|use_spec| match use_spec.match_one(uses) {
            // Allow no match on `routes_to_skip` to support soft transitions.
            Err(MatchOneError::NoMatch(_)) => None,
            result => Some(result.map_err(|err| err.into())),
        })
        .collect::<Result<Vec<&UseDecl>>>()?;
    let routes_to_verify = component_routes
        .routes_to_verify
        .iter()
        .map(|route_match| {
            route_match
                .target
                .match_one(uses)
                .map(|use_decl| Binding { route_match, use_decl })
                .map_err(|err| err.into())
        })
        .collect::<Result<Vec<Binding<'a>>>>()?;

    let mut all_matches = routes_to_skip.clone();
    all_matches.extend(routes_to_verify.iter().map(|binding| binding.use_decl));
    let mut deduped_matches: Vec<&UseDecl> = vec![];
    let mut duped_matches: Vec<&UseDecl> = vec![];
    for i in 0..all_matches.len() {
        if duped_matches.contains(&all_matches[i]) {
            continue;
        }
        for j in i + 1..all_matches.len() {
            if all_matches[i] == all_matches[j] {
                duped_matches.push(all_matches[i]);
                continue;
            }
        }
        deduped_matches.push(all_matches[i]);
    }

    if deduped_matches.len() < uses.len() {
        let missed_routes: Vec<&UseDecl> =
            uses.iter().filter(|use_decl| !deduped_matches.contains(use_decl)).collect();
        if duped_matches.len() > 0 {
            Err(anyhow!(
                "{}. {}; moniker: {} routes matched multiple times: {:#?}; unmatched routes: {:#?}",
                ROUTE_LISTS_OVERLAP,
                ROUTE_LISTS_INCOMPLETE,
                moniker,
                duped_matches,
                missed_routes
            ))
        } else {
            Err(anyhow!(
                "{}; moniker: {}; unmatched routes: {:#?}",
                ROUTE_LISTS_INCOMPLETE,
                moniker,
                missed_routes
            ))
        }
    } else if duped_matches.len() > 0 {
        Err(anyhow!(
            "{}; moniker: {}; routes matched multiple times: {:#?}",
            ROUTE_LISTS_OVERLAP,
            moniker,
            duped_matches
        ))
    } else {
        Ok((routes_to_skip, routes_to_verify))
    }
}

fn check_pkg_source(
    route_segment: &RouteSegment,
    component_model: &Arc<ComponentModelForAnalyzer>,
    components: &Vec<Component>,
) -> Option<RouteSourceError> {
    let moniker = route_segment.moniker();
    if moniker.is_none() {
        return Some(RouteSourceError::RouteSegmentWithoutComponent(route_segment.clone()));
    }
    let moniker = moniker.unwrap();

    let get_instance_result = component_model.get_instance(&moniker);
    if get_instance_result.is_err() {
        return Some(RouteSourceError::RouteSegmentAbsMonikerNotFoundInTree(route_segment.clone()));
    }
    let instance = get_instance_result.unwrap();
    let instance_url = instance.url();

    let matches: Vec<&Component> =
        components.iter().filter(|component| component.url == *instance_url).collect();
    if matches.len() == 0 {
        return Some(RouteSourceError::ComponentInstanceLookupByUrlFailed(
            instance_url.to_string(),
        ));
    }
    if matches.len() > 1 {
        return Some(RouteSourceError::MultipleComponentsWithSameUrl(
            matches.iter().map(|&component| component.clone()).collect(),
        ));
    }

    match &matches[0].source {
        ComponentSource::ZbiBootfs | ComponentSource::StaticPackage(_) => None,
        source => Some(RouteSourceError::RouteSegmentComponentFromUntrustedSource(
            route_segment.clone(),
            source.clone(),
        )),
    }
}

fn process_verify_result<'a>(
    verify_result: VerifyRouteResult,
    route: &Binding<'a>,
    component_model: &Arc<ComponentModelForAnalyzer>,
    components: &Vec<Component>,
) -> Result<Result<Source, RouteSourceError>> {
    match verify_result.error {
        None => {
            for route_segment in verify_result.route.iter() {
                if let Some(err) = check_pkg_source(route_segment, component_model, components) {
                    return Ok(Err(err));
                }
            }

            let route_source = verify_result.route.last().ok_or_else(|| {
                anyhow!(
                    "Route verifier traced empty route for capability matching {:?}",
                    json_or_unformatted(&route.route_match.target, "route target")
                )
            })?;
            if let RouteSegment::DeclareBy { moniker, capability } = route_source {
                let source = Source { moniker: moniker.clone(), capability: capability.clone() };
                let matches_result: Result<Vec<bool>> = vec![
                    route.route_match.source.capability.matches(capability),
                    route.route_match.source.capability.matches(&verify_result.route),
                ]
                .into_iter()
                .map(|r| r)
                .collect();
                match matches_result {
                    Ok(source_and_cap_res) => {
                        if source_and_cap_res[0] && source_and_cap_res[1] {
                            Ok(Ok(source))
                        } else {
                            Ok(Err(RouteSourceError::RouteMismatch(source)))
                        }
                    }
                    Err(_) => Ok(Err(RouteSourceError::RouteMismatch(source))),
                }
            } else {
                Ok(Err(RouteSourceError::MissingSourceCapability(route_source.clone())))
            }
        }
        Some(err) => Ok(Err(RouteSourceError::AnalyzerModelError(err))),
    }
}

/// `DataController` for verifying specific routes used by specific components.
pub struct RouteSourcesController {}

impl RouteSourcesController {
    pub fn get_results(model: Arc<DataModel>, input: String) -> Result<VerifyRouteSourcesResults> {
        let config_data = read_to_string(&input).map_err(|err| {
            anyhow!("Failed to parse config from file: {}: {}", &input, err.to_string())
        })?;
        let config: RouteSourcesConfig = serde_json5::from_str(&config_data).map_err(|err| {
            anyhow!("Failed to parse config from file: {}: {}", &input, err.to_string())
        })?;
        let component_model_result = model.get::<V2ComponentModel>()?;
        let component_model = &component_model_result.component_model;
        let components = &model.get::<Components>()?.entries;
        let results = Self::run(component_model, components, &config)?;
        let deps = component_model_result.deps.clone();
        Ok(VerifyRouteSourcesResults { deps, results })
    }

    fn run(
        component_model: &Arc<ComponentModelForAnalyzer>,
        components: &Vec<Component>,
        config: &RouteSourcesConfig,
    ) -> Result<HashMap<String, Vec<VerifyRouteSourcesResult>>> {
        let mut results = HashMap::new();
        for component_routes in config.component_routes.iter() {
            let target_moniker = &component_routes.target_node_path;
            let target_instance = match component_model.get_instance(target_moniker) {
                Ok(target_instance) => target_instance,
                Err(routing::error::ComponentInstanceError::InstanceNotFound { .. })
                    if component_routes.skip_if_target_node_missing =>
                {
                    continue
                }
                e @ Err(_) => e.context(format!(
                    "{}; target instance: {}",
                    MISSING_TARGET_INSTANCE, target_moniker,
                ))?,
            };

            let (_, routes_to_verify) =
                gather_routes(component_routes, target_instance.decl_for_testing(), target_moniker)
                    .context(format!(
                        "{}; target instance: {}",
                        GATHER_FAILED,
                        target_moniker.clone()
                    ))?;

            let mut component_results = Vec::new();
            for route in routes_to_verify.into_iter() {
                // For some capabilities, a single use declaration can result in 2 route verifications
                // (e.g. for storage capabilities, we check routing for both the storage capability itself
                // and for its backing directory capability.)
                for verify_result in component_model
                    .check_use_capability(route.use_decl, &target_instance)
                    .now_or_never()
                    .unwrap()
                {
                    let result =
                        process_verify_result(verify_result, &route, component_model, components)?;
                    let query = route.route_match.clone();
                    component_results.push(VerifyRouteSourcesResult { query, result });
                }
            }

            results.insert(target_moniker.to_string(), component_results);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Matches, RouteMatch, RouteSourcesConfig, RouteSourcesController, RouteSourcesSpec,
        SourceDeclSpec, SourceSpec, UseSpec, MISSING_TARGET_INSTANCE, ROUTE_LISTS_INCOMPLETE,
        ROUTE_LISTS_OVERLAP,
    };
    use crate::verify::route_sources::{RouteSourceError, Source, VerifyRouteSourcesResult};
    use anyhow::Result;
    use cm_config::RuntimeConfig;
    use cm_fidl_analyzer::component_model::ModelBuilderForAnalyzer;
    use cm_rust::{
        Availability, CapabilityTypeName, DependencyType, DirectoryDecl, ExposeSource, OfferSource,
        ProgramDecl, UseDirectoryDecl, UseSource, UseStorageDecl,
    };
    use cm_rust_testing::*;
    use cm_types::{Path, Url};
    use fidl_fuchsia_io as fio;
    use fuchsia_merkle::{Hash, HASH_SIZE};
    use maplit::{hashmap, hashset};
    use moniker::Moniker;
    use routing::environment::RunnerRegistry;
    use routing::mapper::RouteSegment;
    use scrutiny_collection::core::{Component, ComponentSource, Components};
    use scrutiny_collection::model::DataModel;
    use scrutiny_collection::v2_component_model::V2ComponentModel;
    use scrutiny_collector::component_model::DEFAULT_ROOT_URL;
    use scrutiny_testing::fake::fake_data_model;
    use std::str::FromStr;
    use std::sync::Arc;

    const TEST_URL_PREFIX: &str = "fuchsia-pkg://test.fuchsia.com";

    fn make_test_url(component_name: &str) -> Url {
        Url::new(&format!("{}/{}#meta/{}.cm", TEST_URL_PREFIX, component_name, component_name))
            .expect("test URL to parse")
    }

    #[fuchsia::test]
    fn test_match_some_only() {
        // Match Some(path), ignoring None name.
        assert!(
            UseSpec {
                type_name: CapabilityTypeName::Storage,
                path: Some(Path::from_str("/path").unwrap()),
                name: None,
            }
            .matches(
                &UseStorageDecl {
                    source_name: "name".parse().unwrap(),
                    target_path: Path::from_str("/path").unwrap(),
                    availability: Availability::Required,
                }
                .into()
            )
            .unwrap()
                == true
        );
        // Match Some(name), ignoring None path.
        assert!(
            UseSpec {
                type_name: CapabilityTypeName::Storage,
                path: None,
                name: Some("name".parse().unwrap()),
            }
            .matches(
                &UseStorageDecl {
                    source_name: "name".parse().unwrap(),
                    target_path: Path::from_str("/path").unwrap(),
                    availability: Availability::Required,
                }
                .into()
            )
            .unwrap()
                == true
        );
    }

    macro_rules! ok_unwrap {
        ($actual_result:expr) => {{
            let actual_result = $actual_result;
            let actual_ref = actual_result.as_ref();
            if actual_ref.is_err() {
                println!("Unexpected Err");
                for err in actual_ref.err().unwrap().chain() {
                    println!("    {}", err.to_string());
                }
            }
            actual_result.ok().unwrap()
        }};
    }

    macro_rules! err_unwrap {
        ($actual_result:expr) => {{
            let actual_result = $actual_result;
            let actual_ref = actual_result.as_ref();
            if actual_ref.is_ok() {
                println!("Unexpected Ok");
                println!("    {:#?}", actual_ref.ok().unwrap());
            }
            actual_result.err().unwrap()
        }};
    }

    macro_rules! err_starts_with {
        ($err:expr, $prefix:expr) => {{
            if !$err.root_cause().to_string().starts_with($prefix) {
                println!("Error root cause does not start with \"{}\"", $prefix);
                for e in $err.chain() {
                    println!("    {}", e.to_string());
                }
            }
            assert!($err.root_cause().to_string().starts_with($prefix));
        }};
    }

    macro_rules! err_contains {
        ($err:expr, $substr:expr) => {{
            if !$err.root_cause().to_string().contains($substr) {
                println!("Error root cause does not contain \"{}\"", $substr);
                for e in $err.chain() {
                    println!("    {}", e.to_string());
                }
            }
            assert!($err.root_cause().to_string().contains($substr));
        }};
    }

    fn create_component(url: &Url, source: ComponentSource) -> Component {
        Component { id: 0, url: url.clone(), source }
    }

    // Component tree:
    //
    //   ________root_url________
    //  /                        \
    // two_dir_user_url           one_dir_provider_url
    // + root_url child name:     + root_url child name:
    //     two_dir_user               one_dir_provider
    //
    // Directory routes:
    //
    // - Component URLs prefixed by @
    // - Capability prefixed by $
    // - Directories and subdirectories inside ()
    //   - Subdirectories have no leading /
    //
    // @one_dir_provider_url: $provider_dir(/data/to/user)
    //     -- $exposed_by_provider(provider_subdir) -->
    //   @root_url
    //     -- $routed_from_provider(root_subdir) -->
    //   @two_dir_user_url
    //     -- (user_subdir) --> (/data/from/provider)
    //
    // I.e.,
    //   @one_dir_provider_url(/data/to/user/provider_subdir/root_subdir/user_subdir)
    //   binds to
    //   @two_dir_user_url(/data/from/provider)
    //
    // @root_url: $root_dir(/data/to/user)
    //     -- $routed_from_root(root_subdir) -->
    //   @two_dir_user_url
    //     -- (user_subdir) --> (/data/from/root)
    //
    // I.e.,
    //   @root_url(/data/to/user/root_subdir/user_subdir)
    //   binds to
    //   @two_dir_user_url(/data/from/root)
    fn valid_two_instance_two_dir_tree_model(
        data_model: Option<Arc<DataModel>>,
    ) -> Result<Arc<DataModel>> {
        let two_dir_user_url = make_test_url("two_dir_user");
        let one_dir_provider_url = make_test_url("one_dir_provider");

        let data_model = data_model.unwrap_or(fake_data_model());
        let components = hashmap! {
            DEFAULT_ROOT_URL.clone() => (ComponentDeclBuilder::new()
                .program(ProgramDecl {
                    runner: Some("some_runner".parse().unwrap()),
                    ..ProgramDecl::default()
                })
                .capability(CapabilityBuilder::directory()
                    .name("root_dir")
                    .path("/data/to/user")
                    .rights(fio::Operations::CONNECT))
                .offer(OfferBuilder::directory()
                    .name("exposed_by_provider")
                    .target_name("routed_from_provider")
                    .source_static_child("one_dir_provider")
                    .target_static_child("two_dir_user")
                    .rights(fio::Operations::CONNECT)
                    .subdir("root_subdir"))
                .offer(OfferBuilder::directory()
                    .name("root_dir")
                    .target_name("routed_from_root")
                    .source(OfferSource::Self_)
                    .target_static_child("two_dir_user")
                    .rights(fio::Operations::CONNECT)
                    .subdir("root_subdir"))
                .child(ChildBuilder::new()
                    .name("two_dir_user")
                    .url(two_dir_user_url.as_str()))
                .child(ChildBuilder::new()
                    .name("one_dir_provider")
                    .url(one_dir_provider_url.as_str()))
                .build(),
                None,
            ),
            two_dir_user_url => (ComponentDeclBuilder::new()
                .use_(UseBuilder::directory()
                    .name("routed_from_provider")
                    .path("/data/from/provider")
                    .rights(fio::Operations::CONNECT)
                    .subdir("user_subdir"))
                .use_(UseBuilder::directory()
                    .name("routed_from_root")
                    .path("/data/from/root")
                    .rights(fio::Operations::CONNECT)
                    .subdir("user_subdir"))
                .build(),
                None,
            ),
            one_dir_provider_url => (ComponentDeclBuilder::new()
                .program(ProgramDecl {
                    runner: Some("some_runner".parse().unwrap()),
                    ..ProgramDecl::default()
                })
                .capability(CapabilityBuilder::directory()
                    .name("provider_dir")
                    .path("/data/to/user")
                    .rights(fio::Operations::CONNECT))
                .expose(ExposeBuilder::directory()
                    .name("provider_dir")
                    .target_name("exposed_by_provider")
                    .source(ExposeSource::Self_)
                    .rights(fio::Operations::CONNECT)
                    .subdir("provider_subdir"))
                .build(),
                None,
            ),
        };
        let build_component_model = ModelBuilderForAnalyzer::new(DEFAULT_ROOT_URL.clone()).build(
            components,
            Arc::new(RuntimeConfig::default()),
            Arc::new(component_id_index::Index::default()),
            RunnerRegistry::default(),
        );
        let deps = hashset! {};
        data_model.set(V2ComponentModel::new(
            deps,
            build_component_model.model.expect("component model to build"),
            build_component_model.errors,
        ))?;
        Ok(data_model)
    }

    fn valid_two_instance_two_dir_components_model(
        data_model: Option<Arc<DataModel>>,
    ) -> Result<Arc<DataModel>> {
        let data_model = data_model.unwrap_or(fake_data_model());
        let components = vec![
            create_component(&*DEFAULT_ROOT_URL, ComponentSource::ZbiBootfs),
            create_component(
                &make_test_url("two_dir_user"),
                ComponentSource::StaticPackage(Hash::from([0u8; HASH_SIZE])),
            ),
            create_component(
                &make_test_url("one_dir_provider"),
                ComponentSource::StaticPackage(Hash::from([1u8; HASH_SIZE])),
            ),
        ];
        data_model.set(Components { entries: components })?;
        Ok(data_model)
    }

    fn two_instance_two_dir_components_model_missing_user(
        data_model: Option<Arc<DataModel>>,
    ) -> Result<Arc<DataModel>> {
        let data_model = data_model.unwrap_or(fake_data_model());
        let components = vec![
            create_component(&*DEFAULT_ROOT_URL, ComponentSource::ZbiBootfs),
            create_component(
                &make_test_url("one_dir_provider"),
                ComponentSource::StaticPackage(Hash::from([0u8; HASH_SIZE])),
            ),
        ];
        data_model.set(Components { entries: components })?;
        Ok(data_model)
    }

    fn two_instance_two_dir_components_model_duplicate_user(
        data_model: Option<Arc<DataModel>>,
    ) -> Result<(Arc<DataModel>, Vec<Component>)> {
        let two_dir_user_url = make_test_url("two_dir_user");
        let one_dir_provider_url = make_test_url("one_dir_provider");

        let data_model = data_model.unwrap_or(fake_data_model());
        let components = vec![
            create_component(&*DEFAULT_ROOT_URL, ComponentSource::ZbiBootfs),
            create_component(
                &two_dir_user_url,
                ComponentSource::StaticPackage(Hash::from([0u8; HASH_SIZE])),
            ),
            create_component(
                &two_dir_user_url,
                ComponentSource::StaticPackage(Hash::from([1u8; HASH_SIZE])),
            ),
            create_component(
                &one_dir_provider_url,
                ComponentSource::StaticPackage(Hash::from([3u8; HASH_SIZE])),
            ),
        ];
        data_model.set(Components { entries: components })?;
        Ok((
            data_model,
            vec![
                create_component(
                    &two_dir_user_url,
                    ComponentSource::StaticPackage(Hash::from([0u8; HASH_SIZE])),
                ),
                create_component(
                    &two_dir_user_url,
                    ComponentSource::StaticPackage(Hash::from([1u8; HASH_SIZE])),
                ),
            ],
        ))
    }

    fn two_instance_two_dir_components_model_untrusted_user_source(
        data_model: Option<Arc<DataModel>>,
    ) -> Result<(Arc<DataModel>, ComponentSource)> {
        let data_model = data_model.unwrap_or(fake_data_model());
        let untrusted_source = ComponentSource::Package(Hash::from([0u8; HASH_SIZE]));
        let components = vec![
            create_component(&*DEFAULT_ROOT_URL, ComponentSource::ZbiBootfs),
            create_component(&make_test_url("two_dir_user"), untrusted_source.clone()),
            create_component(
                &make_test_url("one_dir_provider"),
                ComponentSource::StaticPackage(Hash::from([1u8; HASH_SIZE])),
            ),
        ];
        data_model.set(Components { entries: components })?;
        Ok((data_model, untrusted_source))
    }

    #[fuchsia::test]
    fn test_component_routes_target_ok() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Vacuous request: Confirms that @root_url uses no input capabilities.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::root(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![],
            }],
        };
        ok_unwrap!(RouteSourcesController::run(component_model, components, &config));

        Ok(())
    }

    #[fuchsia::test]
    fn test_component_routes_missing_target() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Request checking routes of a component instance that does not exist.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("does/not/exist").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        // Not using `err_start_with` because matching `err` string, not
        // `err.root_cause()` string; `MISSING_TARGET_INSTANCE` is the last context
        // attached to the error, which originates elsewhere.
        assert!(err.to_string().starts_with(MISSING_TARGET_INSTANCE));

        Ok(())
    }

    #[fuchsia::test]
    fn test_component_routes_missing_target_ignored() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Request checking routes of a component instance that does not exist.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("does/not/exist").unwrap(),
                skip_if_target_node_missing: true,
                routes_to_skip: vec![],
                // This route will not be verified but verification itself will succeed because
                // the target node does not exist in the topology and skip_if_target_node_missing
                // is true.
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(result, hashmap! {});

        Ok(())
    }

    #[fuchsia::test]
    fn test_route_lists_incomplete() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Request fails because not all capabilities used by @two_dir_user_url
        // are listed in `routes_to_skip` + `routes_to_verify`.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config,));
        assert!(err.root_cause().to_string().starts_with(ROUTE_LISTS_INCOMPLETE));

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_all_routes() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Successful request that confirms but does not verify sources on all capabilities used by
        // @two_dir_user_url.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/provider").unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                "two_dir_user".to_string() => vec![] as Vec<VerifyRouteSourcesResult>,
            },
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_extra_route() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Successful request that confirms but does not verify sources on all
        // capabilities used by @two_dir_user_url, and lists an extra route to
        // skip. This pattern is permitted to allow for soft transitions where
        // new routes are added to `routes_to_skip` config first, then added to
        // the corresponding component manifest.
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/provider").unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/new/dir/route").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                "two_dir_user".to_string() => vec![] as Vec<VerifyRouteSourcesResult>,
            },
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_match_some_routes() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    // Skip @root_url -> @two_dir_user_url route.
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));

        assert_eq!(
            result,
            hashmap! {
                "two_dir_user".to_string() => vec![
                    VerifyRouteSourcesResult{
                        query: config.component_routes[0].routes_to_verify[0].clone(),
                        result: Ok(Source {
                            moniker: config.component_routes[0].routes_to_verify[0].source.moniker.clone(),
                            capability: DirectoryDecl{
                                name: "provider_dir".parse().unwrap(),
                                source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                rights: fio::Operations::CONNECT,
                            }.into(),
                        })
                    }
                ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_match_some_routes_partial_path() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    // Skip @root_url -> @two_dir_user_url route.
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match partial path with some (not all) routed
                                // subdirs.
                                path_prefix: Some(
                                    Path::from_str("/data/to/user/provider_subdir/root_subdir")
                                        .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[0].clone(),
                            result: Ok(Source {
                                moniker: config.component_routes[0].routes_to_verify[0].source.moniker.clone(),
                                capability: DirectoryDecl{
                                    name: "provider_dir".parse().unwrap(),
                                    source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                    rights: fio::Operations::CONNECT,
                                }.into(),
                            })
                        }
                    ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_match_all_routes() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route: @root_url -> @two_dir_user_url route.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/root").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::root(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str("/data/to/user/root_subdir/user_subdir")
                                        .unwrap(),
                                ),
                                name: Some("root_dir".parse().unwrap()),
                            },
                        },
                    },
                    // config.component_routes[0].routes_to_verify[1]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[0].clone(),
                            result: Ok(Source {
                                moniker: config.component_routes[0].routes_to_verify[0].source.moniker.clone(),
                                capability: DirectoryDecl{
                                    name: "root_dir".parse().unwrap(),
                                    source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                    rights: fio::Operations::CONNECT,
                                }.into(),
                            })
                        },
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[1].clone(),
                            result: Ok(Source {
                                moniker: config.component_routes[0].routes_to_verify[1].source.moniker.clone(),
                                capability: DirectoryDecl{
                                    name: "provider_dir".parse().unwrap(),
                                    source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                    rights: fio::Operations::CONNECT,
                                }.into(),
                            })
                        }
                    ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_match_multiple_components() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![
                // Match empty set of routes used by @root_url.
                RouteSourcesSpec {
                    target_node_path: Moniker::root(),
                    skip_if_target_node_missing: false,
                    routes_to_skip: vec![],
                    routes_to_verify: vec![],
                },
                // Match all routes used by @two_dir_user_url.
                RouteSourcesSpec {
                    target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                    skip_if_target_node_missing: false,
                    routes_to_skip: vec![],
                    routes_to_verify: vec![
                        // config.component_routes[1].routes_to_verify[0]:
                        // Match route: @root_url -> @two_dir_user_url route.
                        RouteMatch {
                            target: UseSpec {
                                type_name: CapabilityTypeName::Directory,
                                path: Some(Path::from_str("/data/from/root").unwrap()),
                                name: None,
                            },
                            source: SourceSpec {
                                moniker: Moniker::root(),
                                capability: SourceDeclSpec {
                                    path_prefix: Some(
                                        Path::from_str("/data/to/user/root_subdir/user_subdir")
                                            .unwrap(),
                                    ),
                                    name: Some("root_dir".parse().unwrap()),
                                },
                            },
                        },
                        // config.component_routes[1].routes_to_verify[1]:
                        // Match route:
                        //   @one_dir_provider_url
                        //     -> @root_url
                        //     -> @one_dir_user_url.
                        RouteMatch {
                            target: UseSpec {
                                type_name: CapabilityTypeName::Directory,
                                path: Some(Path::from_str("/data/from/provider").unwrap()),
                                name: None,
                            },
                            source: SourceSpec {
                                moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                                capability: SourceDeclSpec {
                                    path_prefix: Some(
                                        Path::from_str(
                                            "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                        )
                                        .unwrap(),
                                    ),
                                    name: Some("provider_dir".parse().unwrap()),
                                },
                            },
                        },
                    ],
                },
            ],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                    ".".to_string() => vec![] as Vec<VerifyRouteSourcesResult>,
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[1].routes_to_verify[0].clone(),
                            result: Ok(Source {
                                moniker: config.component_routes[1].routes_to_verify[0].source.moniker.clone(),
                                capability: DirectoryDecl{
                                    name: "root_dir".parse().unwrap(),
                                    source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                    rights: fio::Operations::CONNECT,
                                }.into(),
                            })
                        },
                        VerifyRouteSourcesResult{
                            query: config.component_routes[1].routes_to_verify[1].clone(),
                            result: Ok(Source {
                                moniker: config.component_routes[1].routes_to_verify[1].source.moniker.clone(),
                                capability: DirectoryDecl{
                                    name: "provider_dir".parse().unwrap(),
                                    source_path: Some(Path::from_str("/data/to/user").unwrap()),
                                    rights: fio::Operations::CONNECT,
                                }.into(),
                            })
                        }
                    ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_misconfigured_skip_match_source_name() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let source_name = "routed_from_provider";
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: None,
                        // This is the source name of the route, but
                        // directory uses are matched by target path.
                        name: Some(source_name.parse().unwrap()),
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        // List appears incomplete because `routes_to_skip[0]` fails to match,
        // and `routes_to_skip` is allowed to contain unmatched items to support
        // soft transitions.
        err_starts_with!(err, ROUTE_LISTS_INCOMPLETE);
        // Correct target path should appear in error message; it is the
        // unmatched route (unmatched because source name should not have been
        // specified). Path stores dirname and basename separately; they may appear separately in
        // error string.
        err_contains!(err, "/data/from");
        err_contains!(err, "provider");

        Ok(())
    }

    #[fuchsia::test]
    fn test_misconfigured_skip_match_source_name_and_target_path() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Path stores dirname and basename separately; they may appear separately in error string.
        let target_dirname = "/data/from";
        let target_basename = "provider";
        let target_path = format!("{}/{}", target_dirname, target_basename);
        let source_name = "routed_from_provider";
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        // This is the correct path, but also specifying
                        // source name should cause a failure.
                        path: Some(Path::from_str(&target_path).unwrap()),
                        // This is the source name of the route, but
                        // directory uses are matched by target path.
                        name: Some(source_name.parse().unwrap()),
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        // List appears incomplete because `routes_to_skip[0]` fails to match,
        // and `routes_to_skip` is allowed to contain unmatched items to support
        // soft transitions.
        err_starts_with!(err, ROUTE_LISTS_INCOMPLETE);
        // Correct target path should appear in error message; it is the
        // unmatched route (unmatched because source name should not have been
        // specified).
        err_contains!(err, target_dirname);
        err_contains!(err, target_basename);
        err_contains!(err, source_name);

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_route_with_no_match() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        // Path stores dirname and basename separately; they may appear separately in error string.
        let bad_dirname = "/does/not";
        let bad_basename = "exist";
        let bad_path = format!("{}/{}", bad_dirname, bad_basename);
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/provider").unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                    // Unmatched `routes_to_skip` entry will be skipped; this
                    // pattern is allowed so that soft transitions that update
                    // verification config first and component manifests second
                    // work as intended.
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str(&bad_path).unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(result, hashmap! {"two_dir_user".to_string() => vec![]});

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_duplicate() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let dup_name = "/data/from/root";
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/provider").unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str(dup_name).unwrap()),
                        name: None,
                    },
                    // Intentional error: Duplicate match for same
                    // route-to-skip.
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str(dup_name).unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        err_starts_with!(err, ROUTE_LISTS_OVERLAP);

        Ok(())
    }

    #[fuchsia::test]
    fn test_match_duplicate() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![UseSpec {
                    type_name: CapabilityTypeName::Directory,
                    path: Some(Path::from_str("/data/from/root").unwrap()),
                    name: None,
                }],
                routes_to_verify: vec![
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                    // Intentional error: Duplicate match for same
                    // route-to-verify.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/root").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                path_prefix: Some(Path::from_str("/data/to/user").unwrap()),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        err_starts_with!(err, ROUTE_LISTS_OVERLAP);

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_match_duplicate() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let dup_name = "/data/from/provider";
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str(dup_name).unwrap()),
                        name: None,
                    },
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str("/data/from/root").unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![
                    // Intentional error: Route-to-verify match is duplicate of
                    // a route-to-skip match.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str(dup_name).unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        err_starts_with!(err, ROUTE_LISTS_OVERLAP);

        Ok(())
    }

    #[fuchsia::test]
    fn test_skip_match_duplicate_mixed() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            valid_two_instance_two_dir_components_model(None)?,
        ))?;
        let dup_name = "/data/from/provider";
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![
                    // Intentional error: No match for `/data/from/root`. That
                    // way number of routes to skip + number of routes to verify
                    // checks out, but route not every route is matched.
                    UseSpec {
                        type_name: CapabilityTypeName::Directory,
                        path: Some(Path::from_str(dup_name).unwrap()),
                        name: None,
                    },
                ],
                routes_to_verify: vec![
                    // Intentional error: Route-to-verify match is duplicate of
                    // a route-to-skip match.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str(dup_name).unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let err = err_unwrap!(RouteSourcesController::run(component_model, components, &config));
        err_contains!(err, ROUTE_LISTS_OVERLAP);
        err_contains!(err, ROUTE_LISTS_INCOMPLETE);

        Ok(())
    }

    #[fuchsia::test]
    fn test_verify_all_missing_user_component() -> Result<()> {
        let data_model = valid_two_instance_two_dir_tree_model(Some(
            two_instance_two_dir_components_model_missing_user(None)?,
        ))?;
        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route: @root_url -> @two_dir_user_url route.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/root").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::root(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str("/data/to/user/root_subdir/user_subdir")
                                        .unwrap(),
                                ),
                                name: Some("root_dir".parse().unwrap()),
                            },
                        },
                    },
                    // config.component_routes[0].routes_to_verify[1]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[0].clone(),
                            result: Err(RouteSourceError::ComponentInstanceLookupByUrlFailed(make_test_url("two_dir_user").to_string())),
                        },
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[1].clone(),
                            result: Err(RouteSourceError::ComponentInstanceLookupByUrlFailed(make_test_url("two_dir_user").to_string())),
                        }
                    ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_verify_all_duplicate_user_component() -> Result<()> {
        let (data_model, duplicate_components) =
            two_instance_two_dir_components_model_duplicate_user(None)?;
        let data_model = valid_two_instance_two_dir_tree_model(Some(data_model))?;

        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route: @root_url -> @two_dir_user_url route.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/root").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::root(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str("/data/to/user/root_subdir/user_subdir")
                                        .unwrap(),
                                ),
                                name: Some("root_dir".parse().unwrap()),
                            },
                        },
                    },
                    // config.component_routes[0].routes_to_verify[1]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));
        assert_eq!(
            result,
            hashmap! {
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[0].clone(),
                            result: Err(RouteSourceError::MultipleComponentsWithSameUrl(duplicate_components.clone())),
                        },
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[1].clone(),
                            result: Err(RouteSourceError::MultipleComponentsWithSameUrl(duplicate_components)),
                        }
                    ],
            }
        );

        Ok(())
    }

    #[fuchsia::test]
    fn test_verify_all_untrusted_user_source() -> Result<()> {
        let (data_model, untrusted_source) =
            two_instance_two_dir_components_model_untrusted_user_source(None)?;
        let data_model = valid_two_instance_two_dir_tree_model(Some(data_model))?;

        let component_model = &data_model.get::<V2ComponentModel>()?.component_model;
        let components = &data_model.get::<Components>()?.entries;
        let config = RouteSourcesConfig {
            component_routes: vec![RouteSourcesSpec {
                target_node_path: Moniker::parse_str("two_dir_user").unwrap(),
                skip_if_target_node_missing: false,
                routes_to_skip: vec![],
                routes_to_verify: vec![
                    // config.component_routes[0].routes_to_verify[0]:
                    // Match route: @root_url -> @two_dir_user_url route.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/root").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::root(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str("/data/to/user/root_subdir/user_subdir")
                                        .unwrap(),
                                ),
                                name: Some("root_dir".parse().unwrap()),
                            },
                        },
                    },
                    // config.component_routes[0].routes_to_verify[1]:
                    // Match route:
                    //   @one_dir_provider_url
                    //     -> @root_url
                    //     -> @one_dir_user_url.
                    RouteMatch {
                        target: UseSpec {
                            type_name: CapabilityTypeName::Directory,
                            path: Some(Path::from_str("/data/from/provider").unwrap()),
                            name: None,
                        },
                        source: SourceSpec {
                            moniker: Moniker::parse_str("one_dir_provider").unwrap(),
                            capability: SourceDeclSpec {
                                // Match complete path with routed subdirs.
                                path_prefix: Some(
                                    Path::from_str(
                                        "/data/to/user/provider_subdir/root_subdir/user_subdir",
                                    )
                                    .unwrap(),
                                ),
                                name: Some("provider_dir".parse().unwrap()),
                            },
                        },
                    },
                ],
            }],
        };
        let result = ok_unwrap!(RouteSourcesController::run(component_model, components, &config));

        assert_eq!(
            result,
            hashmap! {
                    "two_dir_user".to_string() => vec![
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[0].clone(),
                            result: Err(RouteSourceError::RouteSegmentComponentFromUntrustedSource(RouteSegment::UseBy {
                                moniker: Moniker::parse_str("two_dir_user").unwrap(),
                                capability: UseDirectoryDecl{
                                    source: UseSource::Parent,
                                    source_name: "routed_from_root".parse().unwrap(),
                                    source_dictionary: Default::default(),
                                    target_path: Path::from_str("/data/from/root").unwrap(),
                                    rights: fio::Operations::CONNECT,
                                    subdir: "user_subdir".parse().unwrap(),
                                    dependency_type: DependencyType::Strong,
                                    availability: Availability::Required,
                                }.into(),
                            }, untrusted_source.clone())),
                        },
                        VerifyRouteSourcesResult{
                            query: config.component_routes[0].routes_to_verify[1].clone(),
                            result: Err(RouteSourceError::RouteSegmentComponentFromUntrustedSource(RouteSegment::UseBy {
                                moniker: Moniker::parse_str("two_dir_user").unwrap(),
                                capability: UseDirectoryDecl{
                                    source: UseSource::Parent,
                                    source_name: "routed_from_provider".parse().unwrap(),
                                    source_dictionary: Default::default(),
                                    target_path: Path::from_str("/data/from/provider").unwrap(),
                                    rights: fio::Operations::CONNECT,
                                    subdir: "user_subdir".parse().unwrap(),
                                    dependency_type: DependencyType::Strong,
                                    availability: Availability::Required,
                                }.into(),
                            }, untrusted_source)),
                        }
                    ],
            }
        );

        Ok(())
    }
}
