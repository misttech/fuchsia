// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, bail};
use log::info;
use persistence_config::Config;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs::{self, File};
use std::io::ErrorKind;

use crate::fetcher::PersistenceData;

const CURRENT_DATA: &str = "/cache/current.json";
const PREVIOUS_DATA: &str = "/cache/previous.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Timestamps {
    // Warning: Persistence stores this information on disk across multiple
    // reboots. These fields' serialization format should be treated as ABI and
    // thus an avenue for breaking changes.
    #[serde(serialize_with = "serialize_boot_time", deserialize_with = "deserialize_boot_time")]
    pub last_sample_boot: zx::BootInstant,
    #[serde(serialize_with = "serialize_utc_time", deserialize_with = "deserialize_utc_time")]
    pub last_sample_utc: fuchsia_runtime::UtcInstant,
}

impl Timestamps {
    pub fn merge(&mut self, other: Self) {
        if self.last_sample_boot < other.last_sample_boot {
            self.last_sample_boot = other.last_sample_boot;
        }
        if self.last_sample_utc < other.last_sample_utc {
            self.last_sample_utc = other.last_sample_utc;
        }
    }
}

fn serialize_boot_time<S: Serializer>(
    time: &zx::BootInstant,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_i64(time.into_nanos())
}

fn deserialize_boot_time<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<zx::BootInstant, D::Error> {
    deserializer.deserialize_i64(TimeNanos).map(zx::BootInstant::from_nanos)
}

fn serialize_utc_time<S: Serializer>(
    time: &fuchsia_runtime::UtcInstant,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_i64(time.into_nanos())
}

fn deserialize_utc_time<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<fuchsia_runtime::UtcInstant, D::Error> {
    deserializer.deserialize_i64(TimeNanos).map(fuchsia_runtime::UtcInstant::from_nanos)
}

/// A visitor that deserializes times as represented by nanoseconds held in a 64-bit integer.
struct TimeNanos;

impl<'de> Visitor<'de> for TimeNanos {
    type Value = i64;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("a 64-bit integer representing time in nanoseconds on an arbitrary timeline")
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        i64::try_from(v).map_err(de::Error::custom)
    }
}

// Forget persisted inspect data from two boots ago, except for tags with
// persist_across_boot enabled.
//
// Persisted inspect data is held in both /cache/current and /cache/previous,
// corresponding to the current and previous boot, respectively. When a boot
// occurs, this function will move /cache/current to /cache/previous then copy
// tags with persist_across_boot back into /cache/current.
pub fn forget_old_data(config: &Config) -> Result<(), Error> {
    info!(
        "Forgetting persisted inspect data from two boots ago, except for tags with persist_across_boot enabled"
    );

    match fs::remove_file(PREVIOUS_DATA) {
        // Works as intended; cache was wiped or doesn't exist yet.
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        // Unknown error
        Err(e) => {
            bail!("Failed to wipe previous data: {e}");
        }
        _ => {}
    }

    if let Err(e) = fs::rename(CURRENT_DATA, PREVIOUS_DATA) {
        if e.kind() == ErrorKind::NotFound {
            return Ok(());
        }
        bail!("Failed to swap current data with previous: {e}");
    }

    let mut data = previous_data()?.context("Data not found; filesystem inconsistency")?;

    remove_tags_without_persist_across_boot(&mut data, config)
        .context("Failed to remove tags without persist_across_boot")?;

    let file = File::create(CURRENT_DATA).context("Failed to open current data")?;
    serde_json::to_writer(file, &data).context("Failed to write current data")
}

fn remove_tags_without_persist_across_boot(
    data: &mut PersistenceData,
    config: &Config,
) -> Result<(), Error> {
    let mut copied_count = 0;

    for (service, service_data) in data.iter_mut() {
        let tags_to_remove = config
            .get(&service.clone())
            .with_context(|| format!("Failed to find service \"{service}\" in config"))?
            .iter()
            .filter(|(_, config)| !config.persist_across_boot)
            .map(|(tag, _)| tag);

        for tag in tags_to_remove {
            service_data.remove(tag);
        }

        copied_count += service_data.len();
    }

    info!("Persisted {copied_count} tags across boot");
    Ok(())
}

fn read_data(path: &str) -> Result<Option<PersistenceData>, Error> {
    match File::open(path) {
        Ok(file) => Ok(serde_json::from_reader(file)
            .with_context(|| format!("Failed to deserialize Persistence data from {path}"))?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => {
            bail!("Failed to read Persistence data from \"{path}\": {e:?}")
        }
    }
}

pub(crate) fn current_data() -> Result<Option<PersistenceData>, Error> {
    read_data(CURRENT_DATA)
}

pub(crate) fn previous_data() -> Result<Option<PersistenceData>, Error> {
    read_data(PREVIOUS_DATA)
}

pub(crate) fn write_current_data(data: &PersistenceData) -> Result<(), Error> {
    let file = File::create(CURRENT_DATA)
        .context("Failed to open current Persistence data for writing")?;
    serde_json::to_writer(file, data).context("Failed to serialize Persistence data")
}

#[cfg(test)]
mod test {
    use super::*;

    fn make_timestamps(nanos: i64) -> Timestamps {
        Timestamps {
            last_sample_boot: zx::BootInstant::from_nanos(nanos),
            last_sample_utc: fuchsia_runtime::UtcInstant::from_nanos(nanos),
        }
    }

    #[fuchsia::test]
    fn test_timestamps_merge() {
        let mut timestamps_1 = make_timestamps(100);
        let timestamps_2 = make_timestamps(200);

        timestamps_1.merge(timestamps_2);

        // timestamps_1 should now have the maximum of each field
        assert_eq!(timestamps_1.last_sample_boot.into_nanos(), 200);
        assert_eq!(timestamps_1.last_sample_utc.into_nanos(), 200);

        let timestamps_3 = make_timestamps(50);
        let mut timestamps_4 = make_timestamps(300);

        timestamps_4.merge(timestamps_3);

        // timestamps_4 should retain its original higher value
        assert_eq!(timestamps_4.last_sample_boot.into_nanos(), 300);
        assert_eq!(timestamps_4.last_sample_utc.into_nanos(), 300);
    }
}
