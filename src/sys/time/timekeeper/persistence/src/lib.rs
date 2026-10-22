// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Testing-only code for persistent Timekeeper behavior changes around
//! real-time clock (RTC) handling.

use anyhow::{Result, anyhow};
use fuchsia_async::{self as fasync, TimeoutExt};
use fuchsia_runtime::UtcTimeline;
use futures::TryFutureExt;
use log::{debug, error};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The path to the internal production persistent state file.
const PERSISTENT_STATE_PATH: &'static str = "/data/persistent_state.json";

/// Maximum time to wait for a persistent state file read or write operation.
const PERSISTENT_STATE_IO_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

/// How many reboots should the setting persist for?
const REBOOT_COUNT: u8 = 1;

/// The persistent Timekeeper state.
///
/// See the method documentation for the specific components of the
/// state that is being persisted.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct State {
    // This many reboots remain until we're allowed to write to RTC again. This
    // counter is decreased by Timekeeper on each restart, until zero is reached.
    num_reboots_until_rtc_write_allowed: u8,

    // If set to nonzero, this is the reference instant on the boot timeline
    // used for estimating UTC.
    //
    // This reverence value MUST be either from this or the immediately preceding boot.
    // This means that we must update this value at least once on each boot to
    // ensure correct handling of the UTC timeline recovery from RTC.
    #[serde(default)]
    reference_boot_instant_ns: i64,

    // If set to nonzero, this is the UTC instant corresponding to
    // `reference_boot_instant_ns` above.
    #[serde(default)]
    reference_utc_instant_ns: i64,
}

impl State {
    pub fn new(update_rtc: bool) -> Self {
        let mut value = Self { ..Default::default() };
        value.set_may_update_rtc(update_rtc);
        value
    }

    /// Returns whether updating the RTC clock is allowed.
    pub fn may_update_rtc(&self) -> bool {
        self.num_reboots_until_rtc_write_allowed == 0
    }

    pub fn set_may_update_rtc(&mut self, update_rtc: bool) {
        self.num_reboots_until_rtc_write_allowed = match update_rtc {
            true => 0,
            // Limit the validity of the setting, expressed in number of
            // reboots.
            false => REBOOT_COUNT,
        };
    }

    /// Gets a known-good reference point on the boot-to-utc affine transform.
    pub fn get_rtc_reference(&self) -> (zx::BootInstant, zx::Instant<UtcTimeline>) {
        let reference_boot = zx::BootInstant::from_nanos(self.reference_boot_instant_ns);
        let reference_utc = zx::Instant::from_nanos(self.reference_utc_instant_ns);
        (reference_boot, reference_utc)
    }

    /// Sets a known-good reference point on the boot-to-utc affine transform.
    ///
    /// This setting can be used to recover a known-good UTC estimate.
    ///
    /// # Args
    ///
    /// - `boot_reference`: a reference instant on the boot timeline.
    /// - `utc_reference`: a reference instant on the UTC timeline that corresponds
    ///   to `boot_timeline`.
    pub fn set_rtc_reference(
        &mut self,
        boot_reference: zx::BootInstant,
        utc_reference: zx::Instant<UtcTimeline>,
    ) {
        self.reference_boot_instant_ns = boot_reference.into_nanos();
        self.reference_utc_instant_ns = utc_reference.into_nanos();
    }

    // Change the reboot limit.
    fn spend_one_reboot_count(&mut self) {
        self.num_reboots_until_rtc_write_allowed =
            self.num_reboots_until_rtc_write_allowed.saturating_sub(1);
    }

    /// Reads the persistent state, updating the testing-only components.
    pub async fn read_and_update() -> Result<Self> {
        Self::read_and_update_internal(PERSISTENT_STATE_PATH).await
    }

    pub async fn read_and_update_internal<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let path_str = path.to_str().ok_or_else(|| anyhow!("non-utf8 path: {path:?}"))?;
        let maybe_state_str: String = fuchsia_fs::file::read_in_namespace_to_string(path_str)
            .map_err(|e| {
                debug!("while reading: {e:?}");
                anyhow::Error::from(e)
            })
            .on_timeout(fasync::MonotonicInstant::after(PERSISTENT_STATE_IO_TIMEOUT), || {
                debug!("timed out reading persistent state from {path:?}");
                Err(anyhow!("timed out reading persistent state from {path:?}"))
            })
            .await?;
        let state: State = serde_json::from_str(&maybe_state_str).map_err(|e| {
            debug!("while deserializing: {e:?}");
            e
        })?;
        // TODO(b/478927688): Convert to `debug!` once fix is confirmed.
        log::info!("read persistent RTC state: {state:?}");
        let mut next = state.clone();
        next.spend_one_reboot_count();
        Self::write_internal(path, &next).await?;
        Ok(state)
    }

    /// Write the persistent state to mutable persistent storage.
    pub async fn write(state: &Self) -> Result<()> {
        Self::write_internal(PERSISTENT_STATE_PATH, state).await
    }

    pub async fn write_internal<P: AsRef<Path>>(path: P, state: &Self) -> Result<()> {
        let path = path.as_ref();
        let path_str = path.to_str().ok_or_else(|| anyhow!("non-utf8 path: {path:?}"))?;
        // TODO(b/478927688): Convert to `debug!` once fix is confirmed.
        log::info!("writing persistent RTC state: {state:?} to {path:?}");
        let state_str = serde_json::to_string(state).map_err(|e| {
            error!("while serializing state: {e:?}");
            e
        })?;
        fuchsia_fs::file::write_in_namespace(path_str, state_str)
            .map_err(|e| {
                error!("while writing persistent state: {path:?}: {e:?}");
                anyhow::Error::from(e)
            })
            .on_timeout(fasync::MonotonicInstant::after(PERSISTENT_STATE_IO_TIMEOUT), || {
                error!("timed out writing persistent state: {path:?}");
                Err(anyhow!("timed out writing persistent state: {path:?}"))
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_io as fio;
    use std::pin::pin;

    #[fuchsia::test]
    async fn test_roundtrip() {
        let d = tempfile::TempDir::new().expect("tempdir created");
        let p = d.path().join("file.json");
        let s = State::new(false);
        State::write_internal(&p, &s).await.unwrap();

        // First time around, we may not update the RTC.
        assert!(!State::read_and_update_internal(&p).await.unwrap().may_update_rtc());
        // Second time around, we may.
        assert!(State::read_and_update_internal(&p).await.unwrap().may_update_rtc());
    }

    #[fuchsia::test]
    fn test_read_and_write_timeout() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();
        executor.set_fake_time(fasync::MonotonicInstant::from_nanos(0));

        // Bind a directory in the local namespace whose server channel is held open without
        // servicing requests so that file operations stall indefinitely until timed out.
        let (dir_client, _dir_server) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
        let ns = fdio::Namespace::installed().expect("installed namespace");
        ns.bind("/stalled_persistence_dir", dir_client).expect("bind stalled dir");

        let path = "/stalled_persistence_dir/state.json";
        let state = State::new(true);

        // 1. Verify write_internal times out after PERSISTENT_STATE_IO_TIMEOUT.
        {
            let mut write_fut = pin!(State::write_internal(path, &state));
            assert!(executor.run_until_stalled(&mut write_fut).is_pending());

            executor.set_fake_time(fasync::MonotonicInstant::after(
                PERSISTENT_STATE_IO_TIMEOUT - zx::MonotonicDuration::from_millis(1),
            ));
            assert!(executor.run_until_stalled(&mut write_fut).is_pending());

            executor.set_fake_time(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(2),
            ));
            let res = executor.run_until_stalled(&mut write_fut);
            match res {
                std::task::Poll::Ready(Err(err)) => {
                    assert!(err.to_string().contains("timed out"), "unexpected error: {err:?}");
                }
                other => panic!("expected Ready(Err), got {other:?}"),
            }
        }

        // 2. Verify read_and_update_internal times out after PERSISTENT_STATE_IO_TIMEOUT.
        {
            let mut read_fut = pin!(State::read_and_update_internal(path));
            assert!(executor.run_until_stalled(&mut read_fut).is_pending());

            executor.set_fake_time(fasync::MonotonicInstant::after(
                PERSISTENT_STATE_IO_TIMEOUT + zx::MonotonicDuration::from_millis(1),
            ));
            let res = executor.run_until_stalled(&mut read_fut);
            match res {
                std::task::Poll::Ready(Err(err)) => {
                    assert!(err.to_string().contains("timed out"), "unexpected error: {err:?}");
                }
                other => panic!("expected Ready(Err), got {other:?}"),
            }
        }

        let _ = ns.unbind("/stalled_persistence_dir");
    }
}
