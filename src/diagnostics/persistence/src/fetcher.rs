// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::file_handler::Timestamps;
use diagnostics_data::{Data, Inspect};
use hashbrown::HashMap;
use persistence_config::{ServiceName, Tag};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::VecDeque;
use std::io;
use std::ops::{Deref, DerefMut};

/// Maximum number of errors to keep for each tag.
pub(crate) const MAX_TAG_ERRORS: usize = 5;
/// Maximum string length of an error stored on disk.
const MAX_ERROR_LEN: usize = 50;

#[derive(Default, Debug, Serialize, Deserialize)]
pub(crate) struct PersistenceData(pub HashMap<ServiceName, ServiceData>);

impl Deref for PersistenceData {
    type Target = HashMap<ServiceName, ServiceData>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for PersistenceData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
pub(crate) struct ServiceData(pub HashMap<Tag, TagData>);

impl Deref for ServiceData {
    type Target = HashMap<Tag, TagData>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ServiceData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct TagData {
    pub data: HashMap<ExtendedMoniker, Data<Inspect>>,
    pub errors: VecDeque<String>,
    pub timestamps: Timestamps,
    pub total_bytes: usize,
    pub max_bytes: usize,
    #[serde(with = "selectors_ext::inspect")]
    pub selectors: Vec<fidl_fuchsia_diagnostics::Selector>,
}

impl TagData {
    pub fn merge(&mut self, timestamps: Timestamps, data: Data<Inspect>) {
        if self.total_bytes > self.max_bytes {
            // Merging is an additive operation and this tag already went over
            // its size quota. Do nothing.
            return;
        }

        self.timestamps.merge(timestamps);

        let moniker = ExtendedMoniker(data.moniker.clone());
        if let Some(existing) = self.data.get_mut(&moniker) {
            existing.merge(data);
        } else {
            self.data.insert(moniker, data);
        }

        self.calculate_total_bytes();
    }

    fn calculate_total_bytes(&mut self) {
        self.total_bytes = 0;

        for data in self.data.values() {
            let data_len = data
                .payload
                .as_ref()
                .and_then(|d| if d.name == "root" { d.children.first() } else { Some(d) })
                .map(|d| {
                    let mut counter = ByteCounter::default();
                    serde_json::to_writer(&mut counter, d).map(|()| counter.count)
                })
                .unwrap_or(Ok(0));

            self.total_bytes += match data_len {
                Ok(len) => len,
                Err(e) => {
                    self.data.clear();
                    self.add_error(format!("Unexpected serialize error: {e}"));
                    return;
                }
            };
        }

        if self.total_bytes > self.max_bytes {
            self.data.clear();
            self.add_error(format!(
                "Data too big: {} > max length {}",
                self.total_bytes, self.max_bytes
            ));
        }
    }

    /// Add an error to the queue with safeguards to prevent error spam from
    /// filling up the disk.
    pub fn add_error(&mut self, mut e: String) {
        e.truncate(MAX_ERROR_LEN);
        self.errors.push_front(e);
        self.errors.truncate(MAX_TAG_ERRORS);
    }
}

#[derive(Eq, Ord, PartialOrd, PartialEq, Debug, Clone, Hash)]
pub(crate) struct ExtendedMoniker(diagnostics_data::ExtendedMoniker);

impl From<diagnostics_data::ExtendedMoniker> for ExtendedMoniker {
    fn from(value: diagnostics_data::ExtendedMoniker) -> Self {
        Self(value)
    }
}

impl Deref for ExtendedMoniker {
    type Target = diagnostics_data::ExtendedMoniker;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for ExtendedMoniker {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtendedMoniker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let moniker_str = String::deserialize(deserializer)?;
        diagnostics_data::ExtendedMoniker::parse_str(&moniker_str)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// ByteCounter is a no-op writer that counts the number of bytes written to it.
#[derive(Default)]
struct ByteCounter {
    count: usize,
}

impl io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.count = self
            .count
            .checked_add(buf.len())
            .ok_or_else::<io::Error, _>(|| io::Error::from(io::ErrorKind::FileTooLarge))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use diagnostics_data::{InspectDataBuilder, Timestamp, hierarchy};
    use hashbrown::HashMap;
    use std::collections::VecDeque;

    fn make_tag_data(max_bytes: usize) -> TagData {
        TagData {
            data: HashMap::new(),
            errors: VecDeque::new(),
            timestamps: make_timestamps(0),
            total_bytes: 0,
            max_bytes,
            selectors: vec![],
        }
    }

    fn make_timestamps(nanos: i64) -> Timestamps {
        Timestamps {
            last_sample_boot: zx::BootInstant::from_nanos(nanos),
            last_sample_utc: fuchsia_runtime::UtcInstant::from_nanos(nanos),
        }
    }

    #[fuchsia::test]
    fn test_tag_data_merge_ignores_when_full() {
        let mut tag_data = make_tag_data(10); // Small limit to trigger overflow easily

        let moniker = diagnostics_data::ExtendedMoniker::parse_str("moniker").unwrap();
        tag_data.merge(
            make_timestamps(100),
            InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(100))
                .with_hierarchy(hierarchy! { root: { child: { x: 1 } } }) // Size will be > 10
                .build(),
        );

        assert!(tag_data.total_bytes > tag_data.max_bytes);
        assert_eq!(tag_data.data, HashMap::new());
        assert!(!tag_data.errors.is_empty());

        let initial_errors_count = tag_data.errors.len();

        // Second merge should be ignored
        let data_ignored =
            InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(200))
                .with_hierarchy(hierarchy! { root: { child: { x: 2 } } })
                .build();

        // Even with newer timestamp, it should be ignored
        tag_data.merge(make_timestamps(200), data_ignored);

        // State should be unchanged; check timestamp wasn't updated
        assert_eq!(tag_data.timestamps.last_sample_boot.into_nanos(), 100);
        // Errors count shouldn't change since no new error were added
        assert_eq!(tag_data.errors.len(), initial_errors_count);
    }

    #[fuchsia::test]
    fn test_tag_data_merge_distinct_monikers() {
        let mut tag_data = make_tag_data(1000);

        let moniker1 = diagnostics_data::ExtendedMoniker::parse_str("moniker1").unwrap();
        let data1 = InspectDataBuilder::new(moniker1.clone(), "url", Timestamp::from_nanos(100))
            .with_hierarchy(hierarchy! { root: { child: { x: 1 } } })
            .build();

        let moniker2 = diagnostics_data::ExtendedMoniker::parse_str("moniker2").unwrap();
        let data2 = InspectDataBuilder::new(moniker2.clone(), "url", Timestamp::from_nanos(100))
            .with_hierarchy(hierarchy! { root: { child: { x: 2 } } })
            .build();

        tag_data.merge(make_timestamps(100), data1.clone());
        let size_1 = tag_data.total_bytes;
        assert!(size_1 > 0);
        assert_eq!(
            tag_data.data,
            HashMap::from([(ExtendedMoniker(moniker1.clone()), data1.clone())])
        );

        tag_data.merge(make_timestamps(100), data2.clone());

        assert_eq!(
            tag_data.data,
            HashMap::from([
                (ExtendedMoniker(moniker1.clone()), data1),
                (ExtendedMoniker(moniker2), data2)
            ])
        );
        assert!(tag_data.total_bytes > size_1);
    }

    #[fuchsia::test]
    fn test_tag_data_merges_data_for_same_moniker() {
        use diagnostics_data::{InspectDataBuilder, Timestamp, hierarchy};

        let mut tag_data = make_tag_data(1000);

        let moniker = diagnostics_data::ExtendedMoniker::parse_str("moniker").unwrap();
        let data1 = InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(100))
            .with_hierarchy(hierarchy! { root: { child: { x: 1 } } })
            .build();

        tag_data.merge(make_timestamps(100), data1);

        let data2 = InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(200))
            .with_hierarchy(hierarchy! { root: { child: { x: 2, y: 3 } } })
            .build();

        tag_data.merge(make_timestamps(200), data2);

        // Verify that data was merged (x updated to 2, y added)
        let expected_data =
            InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(200))
                .with_hierarchy(hierarchy! { root: { child: { x: 2, y: 3 } } })
                .build();

        assert_eq!(tag_data.data, HashMap::from([(ExtendedMoniker(moniker), expected_data)]));

        assert_eq!(tag_data.timestamps.last_sample_boot.into_nanos(), 200);
        assert!(tag_data.total_bytes > 0);
    }

    #[fuchsia::test]
    fn test_tag_data_calculate_total_bytes() {
        let mut tag_data = make_tag_data(1000);

        let moniker = diagnostics_data::ExtendedMoniker::parse_str("moniker").unwrap();
        let data = InspectDataBuilder::new(moniker.clone(), "url", Timestamp::from_nanos(100))
            .with_hierarchy(hierarchy! { root: { child: { x: 1 } } })
            .build();

        tag_data.data.insert(ExtendedMoniker(moniker), data);
        tag_data.calculate_total_bytes();

        let initial_bytes = tag_data.total_bytes;
        assert!(initial_bytes > 0);
        assert!(tag_data.errors.is_empty());

        // Now reduce max_bytes to force overflow
        tag_data.max_bytes = initial_bytes - 1;
        tag_data.calculate_total_bytes();

        assert_eq!(tag_data.total_bytes, initial_bytes);
        assert_eq!(tag_data.data, HashMap::new());
        let expected_errors = VecDeque::from([format!(
            "Data too big: {} > max length {}",
            initial_bytes, tag_data.max_bytes
        )]);
        assert_eq!(tag_data.errors, expected_errors);
    }

    #[fuchsia::test]
    fn test_tag_data_max_errors() {
        let mut tag_data = make_tag_data(1000);

        for i in 0..10 {
            tag_data.add_error(format!("Error {}", i));
        }

        let expected_errors = VecDeque::from([
            "Error 9".to_string(),
            "Error 8".to_string(),
            "Error 7".to_string(),
            "Error 6".to_string(),
            "Error 5".to_string(),
        ]);
        assert_eq!(tag_data.errors, expected_errors);
    }

    #[fuchsia::test]
    fn test_tag_data_error_truncation() {
        let mut tag_data = make_tag_data(1000);

        // Test truncation of long error
        let long_error = "a".repeat(MAX_ERROR_LEN + 10);
        tag_data.add_error(long_error);
        assert_eq!(tag_data.errors[0].len(), MAX_ERROR_LEN);
    }
}
