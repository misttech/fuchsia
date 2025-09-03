// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::file_handler::{self, PersistData, PersistPayload, PersistSchema, Timestamps};
use crate::scheduler::TagState;
use anyhow::{Context, Error, bail};
use diagnostics_data::{Data, DiagnosticsHierarchy, ExtendedMoniker, Inspect};
use diagnostics_reader::{ArchiveReader, RetryConfig};
use fidl_fuchsia_diagnostics as fdiagnostics;
use log::*;
use persistence_config::{ServiceName, Tag};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::collections::hash_map::Entry;

// The capability name for the Inspect reader
const INSPECT_SERVICE_PATH: &str = "/svc/fuchsia.diagnostics.ArchiveAccessor.feedback";

fn extract_json_map(hierarchy: DiagnosticsHierarchy) -> Option<Map<String, Value>> {
    let Ok(Value::Object(mut map)) = serde_json::to_value(hierarchy) else {
        return None;
    };
    if map.len() != 1 || !map.contains_key("root") {
        return Some(map);
    }
    if let Value::Object(map) = map.remove("root").unwrap() {
        return Some(map);
    }
    None
}

#[derive(Debug, Eq, PartialEq)]
struct DataMap(HashMap<ExtendedMoniker, Value>);

impl Serialize for DataMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (moniker, value) in &self.0 {
            map.serialize_entry(&moniker.to_string(), &value)?;
        }
        map.end()
    }
}

impl std::ops::Deref for DataMap {
    type Target = HashMap<ExtendedMoniker, Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn condensed_map_of_data(items: impl IntoIterator<Item = Data<Inspect>>) -> DataMap {
    DataMap(items.into_iter().fold(HashMap::new(), |mut entries, item| {
        let Data { payload, moniker, .. } = item;
        if let Some(new_map) = payload.and_then(extract_json_map) {
            match entries.entry(moniker) {
                Entry::Occupied(mut o) => {
                    let existing_payload = o.get_mut();
                    if let Value::Object(existing_payload_map) = existing_payload {
                        existing_payload_map.extend(new_map);
                    }
                }
                Entry::Vacant(v) => {
                    v.insert(Value::Object(new_map));
                }
            }
        }
        entries
    }))
}

fn save_data_for_tag(
    inspect_data: Vec<Data<Inspect>>,
    timestamps: Timestamps,
    tag: &Tag,
    selectors: &[fdiagnostics::Selector],
    max_bytes: usize,
) -> PersistSchema {
    let mut filtered_datas = vec![];
    for data in inspect_data {
        match data.filter(selectors) {
            Ok(Some(data)) => filtered_datas.push(data),
            Ok(None) => {}
            Err(e) => return PersistSchema::error(timestamps, format!("Filter error: {e}")),
        }
    }

    if filtered_datas.is_empty() {
        return PersistSchema::error(timestamps, format!("No data available for tag '{tag}'"));
    }
    // We may have multiple entries with the same moniker. Fold those together into a single entry.
    let entries = condensed_map_of_data(filtered_datas);
    let data_length = match serde_json::to_string(&entries) {
        Ok(string) => string.len(),
        Err(e) => {
            return PersistSchema::error(timestamps, format!("Unexpected serialize error: {e}"));
        }
    };
    if data_length > max_bytes {
        let error_description = format!("Data too big: {data_length} > max length {max_bytes}");
        return PersistSchema::error(timestamps, error_description);
    }
    PersistSchema {
        timestamps,
        payload: PersistPayload::Data(PersistData { data_length, entries: entries.0 }),
    }
}

fn utc_now() -> i64 {
    let now_utc = chrono::prelude::Utc::now(); // Consider using SystemTime::now()?
    now_utc.timestamp() * 1_000_000_000 + now_utc.timestamp_subsec_nanos() as i64
}

pub(crate) async fn fetch_and_save<'a, P>(
    services_state: &'a HashMap<ServiceName, HashMap<Tag, TagState>>,
    pending: P,
) -> Result<(), Error>
where
    P: IntoIterator<Item = (&'a ServiceName, &'a Vec<Tag>)>,
    P::IntoIter: Clone,
{
    let pending_services = pending.into_iter().filter_map(|(service, tags)| {
        services_state
            .get(service)
            .or_else(|| {
                warn!("Skipping fetch request for unknown service \"{service}\"");
                None
            })
            .map(move |service_state| {
                let tag_states = tags.iter().filter_map(move |tag| {
                    service_state.get(tag).or_else(|| {
                        warn!("Skipping fetch request for unknown tag \"{tag}\" (service \"{service}\")");
                        None
                    }).map(|state| (tag, state))
                });
                (service, tag_states)
            })
    });

    let selectors: Vec<fdiagnostics::Selector> = pending_services
        .clone()
        .flat_map(|(_, tags)| tags)
        .flat_map(|(_, tag_state)| tag_state.selectors.clone())
        // TODO(https://fxbug.dev/438817180): Use `.peekable()` to avoid unnecessary allocations.
        .collect();

    if selectors.is_empty() {
        bail!("Nothing to fetch! This shouldn't ever happen; please file a bug");
    }

    let proxy = fuchsia_component::client::connect_to_protocol_at_path::<
        fdiagnostics::ArchiveAccessorMarker,
    >(INSPECT_SERVICE_PATH)?;
    let mut source = ArchiveReader::inspect();
    source
        .with_archive(proxy.clone())
        .retry(RetryConfig::never())
        .add_selectors(selectors.into_iter());

    // Do the fetch and record the timestamps.
    let before_utc = utc_now();
    let before_monotonic = zx::MonotonicInstant::get().into_nanos();
    let data = source.snapshot().await.context("Failed to fetch Inspect data")?;
    let after_utc = utc_now();
    let after_monotonic = zx::MonotonicInstant::get().into_nanos();
    let timestamps = Timestamps { before_utc, before_monotonic, after_utc, after_monotonic };

    // Process the data for each tag
    for (service, pending_tags) in pending_services {
        for (tag, state) in pending_tags {
            let data_to_save = save_data_for_tag(
                data.clone(),
                timestamps.clone(),
                tag,
                &state.selectors,
                state.max_bytes,
            );
            file_handler::write(service, tag, &data_to_save);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_data::{InspectDataBuilder, InspectHandleName, Timestamp};
    use diagnostics_hierarchy::hierarchy;
    use serde_json::json;

    #[fuchsia::test]
    fn test_condense_empty() {
        let empty_data = InspectDataBuilder::new(
            "a/b/c/d".try_into().unwrap(),
            "fuchsia-pkg://test",
            Timestamp::from_nanos(123456i64),
        )
        .with_name(InspectHandleName::filename("test_file_plz_ignore.inspect"))
        .build();
        let empty_data_result = condensed_map_of_data([empty_data]);
        let empty_vec_result = condensed_map_of_data([]);

        let expected_map = HashMap::new();

        pretty_assertions::assert_eq!(*empty_data_result, expected_map, "golden diff failed.");
        pretty_assertions::assert_eq!(*empty_vec_result, expected_map, "golden diff failed.");
    }

    fn make_data(mut hierarchy: DiagnosticsHierarchy, moniker: &str) -> Data<Inspect> {
        hierarchy.sort();
        InspectDataBuilder::new(
            moniker.try_into().unwrap(),
            "fuchsia-pkg://test",
            Timestamp::from_nanos(123456i64),
        )
        .with_hierarchy(hierarchy)
        .with_name(InspectHandleName::filename("test_file_plz_ignore.inspect"))
        .build()
    }

    #[fuchsia::test]
    fn test_condense_one() {
        let data = make_data(
            hierarchy! {
                root: {
                    "x": "foo",
                    "y": "bar",
                }
            },
            "a/b/c/d",
        );

        let expected_json = json!({
            "a/b/c/d": {
                "x": "foo",
                "y": "bar",
            }
        });

        let result = condensed_map_of_data([data]);

        pretty_assertions::assert_eq!(
            serde_json::to_value(&result).unwrap(),
            expected_json,
            "golden diff failed."
        );
    }

    #[fuchsia::test]
    fn test_condense_several_with_merge() {
        let data_abcd = make_data(
            hierarchy! {
                root: {
                    "x": "foo",
                    "y": "bar",
                }
            },
            "a/b/c/d",
        );
        let data_efgh = make_data(
            hierarchy! {
                root: {
                    "x": "ex",
                    "y": "why",
                }
            },
            "e/f/g/h",
        );
        let data_abcd2 = make_data(
            hierarchy! {
                root: {
                    "x": "X",
                    "z": "zebra",
                }
            },
            "a/b/c/d",
        );

        let expected_json = json!({
            "a/b/c/d": {
                "x": "X",
                "y": "bar",
                "z": "zebra",
            },
            "e/f/g/h": {
                "x": "ex",
                "y": "why"
            }
        });

        let result = condensed_map_of_data(vec![data_abcd, data_efgh, data_abcd2]);

        pretty_assertions::assert_eq!(
            serde_json::to_value(&result).unwrap(),
            expected_json,
            "golden diff failed."
        );
    }

    const TIMESTAMPS: Timestamps =
        Timestamps { after_monotonic: 200, after_utc: 111, before_monotonic: 100, before_utc: 110 };

    fn parse_selectors(selectors: &'static [&'static str]) -> Vec<fdiagnostics::Selector> {
        selectors
            .iter()
            .map(|s| selectors::parse_selector::<selectors::VerboseError>(s))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[fuchsia::test]
    fn save_data_no_data() {
        let tag = Tag::new("tag".to_string()).unwrap();
        let result = save_data_for_tag(
            vec![],
            TIMESTAMPS.clone(),
            &tag,
            &parse_selectors(&["moniker:path:property"]),
            1000,
        );

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "@timestamps": {
                    "after_monotonic": 200,
                    "after_utc": 111,
                    "before_monotonic": 100,
                    "before_utc": 110,
                },
                ":error": {
                    "description": "No data available for tag 'tag'",
                },
            })
        );
    }

    #[fuchsia::test]
    fn save_data_too_big() {
        let tag = Tag::new("tag".to_string()).unwrap();
        let data_abcd = make_data(
            hierarchy! {
                root: {
                    "x": "foo",
                    "y": "bar",
                }
            },
            "a/b/c/d",
        );

        let result = save_data_for_tag(
            vec![data_abcd],
            TIMESTAMPS.clone(),
            &tag,
            &parse_selectors(&["a/b/c/d:root:y"]),
            20,
        );

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "@timestamps": {
                    "after_monotonic": 200,
                    "after_utc": 111,
                    "before_monotonic": 100,
                    "before_utc": 110,
                },
                ":error": {
                    "description": "Data too big: 23 > max length 20",
                },
            })
        );
    }

    #[fuchsia::test]
    fn save_string_with_data() {
        let tag = Tag::new("tag".to_string()).unwrap();
        let data_abcd = make_data(
            hierarchy! {
                root: {
                    "x": "foo",
                    "y": "bar",
                }
            },
            "a/b/c/d",
        );
        let data_efgh = make_data(
            hierarchy! {
                root: {
                    "x": "ex",
                    "y": "why",
                }
            },
            "e/f/g/h",
        );
        let data_abcd2 = make_data(
            hierarchy! {
                root: {
                    "x": "X",
                    "z": "zebra",
                }
            },
            "a/b/c/d",
        );

        let result = save_data_for_tag(
            vec![data_abcd, data_efgh, data_abcd2],
            TIMESTAMPS.clone(),
            &tag,
            &parse_selectors(&["a/b/c/d:root:y"]),
            1000,
        );

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({
                "@timestamps": {
                    "after_monotonic": 200,
                    "after_utc": 111,
                    "before_monotonic": 100,
                    "before_utc": 110,
                },
                "@persist_size": 23,
                "a/b/c/d": {
                    "y": "bar",
                },
            })
        );
    }
}
