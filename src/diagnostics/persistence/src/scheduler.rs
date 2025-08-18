// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fetcher;
use fuchsia_sync::Mutex;
use log::{error, warn};
use persistence_config::{Config, ServiceName, Tag, TagConfig};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use {
    fidl_fuchsia_diagnostics as fdiagnostics, fidl_fuchsia_diagnostics_persist as fpersist,
    fuchsia_async as fasync,
};

// This contains the logic to decide which tags to fetch at what times. It contains the state of
// each tag (when last fetched, whether currently queued). When a request arrives via FIDL, it's
// sent here and results in requests queued to the Fetcher.

// Selectors for Inspect data must start with this exact string.
const INSPECT_PREFIX: &str = "INSPECT:";

/// Tracks when each tag was persisted last, as necessary for implementing
/// debounce on [`TagConfig`]'s `min_seconds_between_fetch`.
#[derive(Clone)]
pub(crate) struct Scheduler {
    scope: fasync::ScopeHandle,

    /// Registry of all tags with additional metadata necessary for scheduling
    /// fetches from Inspect.
    ///
    /// TODO(https://fxbug.dev/437989316): Save memory using Vec instead.
    service_state: Arc<HashMap<ServiceName, HashMap<Tag, TagState>>>,

    /// Collection of which tags are scheduled to be fetched from Inspect.
    fetch_schedule: Arc<Mutex<BTreeMap<zx::MonotonicInstant, ServiceTags>>>,
}

type ServiceTags = Vec<(ServiceName, Tag)>;

/// Compilation of [`TagConfig`] with additional tracking when this tag was last
/// persisted.
pub(crate) struct TagState {
    pub selectors: Vec<fdiagnostics::Selector>,
    pub max_bytes: usize,
    backoff: zx::MonotonicDuration,
    state: Mutex<TagFetchState>,
}

impl TryFrom<&TagConfig> for TagState {
    type Error = selectors::Error;
    fn try_from(value: &TagConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            selectors: value
                .selectors
                .iter()
                .filter_map(strip_inspect_prefix)
                .map(selectors::parse_verbose)
                .collect::<Result<Vec<_>, _>>()?,
            max_bytes: value.max_bytes,
            backoff: zx::MonotonicDuration::from_seconds(value.min_seconds_between_fetch),
            state: Mutex::new(TagFetchState::ReadyAfter(zx::MonotonicInstant::INFINITE_PAST)),
        })
    }
}

fn strip_inspect_prefix(selector: &String) -> Option<&str> {
    if &selector[..INSPECT_PREFIX.len()] == INSPECT_PREFIX {
        Some(&selector[INSPECT_PREFIX.len()..])
    } else {
        warn!("Selector does not begin with \"INSPECT:\": {selector}");
        None
    }
}

/// Scheduler error.
#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    #[error("unknown service name \"{0}\"")]
    UnknownService(ServiceName),
    #[error("unknown tag name \"{tag}\" for service \"{service}\"")]
    UnknownTag { service: ServiceName, tag: Tag },
    #[error("invalid tag name \"{0}\" must match [a-z][a-z-]*")]
    InvalidTag(String),
    #[error("invalid selector")]
    InvalidSelector(#[from] selectors::Error),
}

impl From<Error> for fpersist::PersistResult {
    fn from(value: Error) -> Self {
        match value {
            Error::UnknownService(_) => Self::BadName,
            Error::UnknownTag { .. } => Self::BadName,
            Error::InvalidTag(_) => Self::BadName,
            Error::InvalidSelector(_) => Self::InternalError,
        }
    }
}

impl Scheduler {
    pub(crate) fn new(scope: fasync::ScopeHandle, config: &Config) -> Result<Self, Error> {
        let mut service_state = HashMap::with_capacity(config.len());
        for (service, tags) in config {
            let mut tag_state = HashMap::with_capacity(tags.len());
            for (tag, tag_config) in tags {
                tag_state.insert(tag.clone(), tag_config.try_into()?);
            }
            service_state.insert(service.clone(), tag_state);
        }
        Ok(Scheduler {
            scope,
            service_state: Arc::new(service_state),
            fetch_schedule: Default::default(),
        })
    }

    /// Gets a service name and a list of valid tags, and queues any fetches that are not already
    /// pending. Updates the last-fetched time on any tag it queues, setting it equal to the later
    /// of the current time and the time the fetch becomes possible.
    pub(crate) fn schedule(
        &self,
        service: &ServiceName,
        tags: impl IntoIterator<Item = String>,
    ) -> Vec<Result<(), Error>> {
        // Every tag we process should use the same Now
        let now = zx::MonotonicInstant::get();
        let Some(tag_states) = self.service_state.get(service) else {
            return tags.into_iter().map(|_| Err(Error::UnknownService(service.clone()))).collect();
        };

        // Filter tags that need to be fetch now from those that need to be
        // fetched later. Group later tags by their next_fetch time using a
        // b-tree, making it efficient to iterate over these batches in
        // order of next_fetch time.
        let (response, now_tags) = {
            let mut now_tags = Vec::new();
            let mut schedule = self.fetch_schedule.lock();
            let response: Vec<Result<(), Error>> = tags
                .into_iter()
                .map(|tag_raw| {
                    let tag = Tag::new(tag_raw.clone()).map_err(|_| Error::InvalidTag(tag_raw))?;
                    tag_states
                        .get(&tag)
                        .ok_or_else(|| Error::UnknownTag {
                            service: service.clone(),
                            tag: tag.clone(),
                        })
                        .map(|tag_state| {
                            let mut state = tag_state.state.lock();
                            match *state {
                                TagFetchState::ReadyAfter(wait_until) => {
                                    if wait_until <= now {
                                        // Debounce period has elapsed; fetch this tag immediately.
                                        now_tags.push(tag);
                                        *state = TagFetchState::ReadyAfter(now + tag_state.backoff);
                                    } else {
                                        // Debounce is still active; schedule this tag for later fetch.
                                        schedule
                                            .entry(wait_until)
                                            .or_default()
                                            .push((service.clone(), tag));
                                        *state = TagFetchState::Scheduled;
                                    }
                                }
                                TagFetchState::Scheduled => {
                                    // This tag has already been scheduled; no action required.
                                }
                            }
                        })
                })
                .collect();
            (response, now_tags)
        };

        if !now_tags.is_empty() {
            let service_state = self.service_state.clone();
            let service = service.clone();
            self.scope.spawn(async move {
                let pending = [(&service, &now_tags)];
                if let Err(e) = fetcher::fetch_and_save(&service_state, pending).await {
                    error!("Failed to fetch inspect: {e:?}");
                }
            });
        }

        self.schedule_next_batch();

        response
    }

    /// Spawn a task to check if there are any pending fetches.
    fn schedule_next_batch(&self) {
        let Some(next_fetch) = self.fetch_schedule.lock().first_entry().map(|e| *e.key()) else {
            // No pending fetches; nothing to do.
            return;
        };

        // Schedule a task to fetch them all at the same time.
        let schedule = self.fetch_schedule.clone();
        let service_state = self.service_state.clone();
        self.scope.spawn(async move {
            let wait = next_fetch - zx::MonotonicInstant::get();
            fasync::Timer::new(wait).await;

            // Collect pending tags to fetch from Inspect.
            let pending = {
                let mut pending: HashMap<ServiceName, Vec<Tag>> = HashMap::new();
                let mut schedule = schedule.lock();
                let now = zx::MonotonicInstant::get();
                while let Some(entry) = schedule.first_entry() {
                    if *entry.key() > now {
                        break;
                    }
                    for (service, tag) in entry.remove().into_iter() {
                        let TagState { state, backoff, .. } = service_state
                            .get(&service)
                            // SAFETY: Config cannot change during runtime.
                            .expect("Missing service")
                            .get(&tag)
                            // SAFETY: Config cannot change during runtime.
                            .expect("Missing tag");
                        *state.lock() = TagFetchState::ReadyAfter(now + *backoff);
                        pending.entry(service).or_default().push(tag);
                    }
                }
                pending
            };

            if pending.is_empty() {
                return;
            }

            if let Err(e) = fetcher::fetch_and_save(&service_state, &pending).await {
                error!("Failed to fetch pending tags from Inspect: {e:?}");
            }
        });
    }
}

/// Tracks when a tag is ready to be fetched again.
enum TagFetchState {
    /// Tag is ready to be fetched after this time.
    ReadyAfter(zx::MonotonicInstant),
    /// Tag is scheduled to be fetched.
    Scheduled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_selector_stripping() {
        assert_eq!(
            ["INSPECT:foo".to_string(), "oops:bar".to_string(), "INSPECT:baz".to_string()]
                .iter()
                .filter_map(strip_inspect_prefix)
                .collect::<Vec<_>>(),
            ["foo".to_string(), "baz".to_string()]
        )
    }
}
