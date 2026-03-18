// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::pin::{Pin, pin};

use assert_matches::assert_matches;
use component_events::descriptor::EventDescriptor;
use component_events::events::{Event, EventStream, EventStreamError, Started, Stopped};
use component_events::matcher::EventMatcher;
use diagnostics_reader::{ArchiveReader, Inspect, RetryConfig, ToSelectorArguments};
use fidl_fuchsia_component::BinderMarker;
use fidl_fuchsia_samplertestcontroller::SamplerTestControllerProxy;
use fidl_test_persistence_factory::ControllerProxy;
use fuchsia_async as fasync;
use fuchsia_component_test::RealmInstance;
use futures::stream::Fuse;
use futures::{FutureExt, StreamExt, select};
use log::{debug, warn};
use pretty_assertions::StrComparison;

use serde_json::Value;
use zx::{MonotonicDuration, MonotonicInstant};

mod mock_fidl;
mod mock_filesystems;
mod test_topology;

/// Name of the service defined in all test_data configs.
const SERVICE: &str = "test-service";

/// Name of the tag defined in all test_data configs.
const TAG: &str = "test-component-metric";

// When to give up on polling for a change and fail the test.
//
// For development it may be convenient to set this to 5. For production, slow
// virtual devices may cause test flakes even with surprisingly long timeouts.
const GIVE_UP_POLLING_SECS: i64 = 120;

const METADATA_KEY: &str = "metadata";
const TIMESTAMP_METADATA_KEY: &str = "timestamp";

// Each persisted tag contains a "@timestamps" object with four timestamps that need to be zeroed.
const PAYLOAD_KEY: &str = "payload";
const ROOT_KEY: &str = "root";
const PERSIST_KEY: &str = "persist";
const TIMESTAMP_STRUCT_KEY: &str = "@timestamps";
const PUBLISHED_TIME_KEY: &str = "published";
const TIMESTAMP_STRUCT_ENTRIES: [&str; 2] = ["last_sample_boot", "last_sample_utc"];
/// If the "samples" Inspect source is publishing Inspect data, the stringified JSON
/// version of that data should include this string. Waiting for it to appear avoids a race
/// condition.
const KEY_FROM_INSPECT_SOURCE: &str = "samples";

enum Published {
    /// No Inspect data is available.
    Waiting,
    /// Inspect data is available but it's empty.
    Empty,
    /// Inspect data is available and populated with the default state of
    /// single_counter_test_component.
    Default,
    /// Custom Inspect data of single_counter_test_component has been set with a
    /// specific integer.
    Int(i64),
    /// Inspect data is available and equal to the specified JSON content.
    /// `persist_size` needs to be calculated manually when using this.
    Content(String),
    /// Publishing failed due to the size of the persisted data exceeded the
    /// configured maximum.
    SizeError(i64),
}

struct TestRealm {
    options: TestRealmOptions,
    instance: RealmInstance,
    inspect: SamplerTestControllerProxy,
    controller: ControllerProxy,
}

/// Inspect data should be persisted through the current boot then published on
/// the next boot.
#[fuchsia::test]
async fn persist_simple() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    // Persistence should publish the Inspect state.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;
}

/// A component overwriting their Inspect data with new values MUST reflect in
/// persisted data.
#[fuchsia::test]
async fn overwrite_with_some() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    // Wait at least one sample period.
    zx::MonotonicDuration::from_seconds(2).sleep();

    // Persistence should overwrite persisted data with the new state after the
    // sample period.
    realm.set_inspect(Some(20i64)).await;
    zx::MonotonicDuration::from_seconds(2).sleep();

    // Persistence should publish the Inspect state.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(20)).await;
}

/// A component un-publishing its Inspect data MUST NOT remove previously
/// persisted data. If a component crashes, its persisted Inspect state needs to
/// remain for debugging.
#[fuchsia::test]
async fn overwrite_with_none() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    // Wait at least one sample period.
    zx::MonotonicDuration::from_seconds(2).sleep();

    // Persistence should not remove existing persisted data if not found.
    realm.set_inspect(None).await;
    zx::MonotonicDuration::from_seconds(2).sleep();

    // Persistence should publish the Inspect state.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;
}

/// Tags are updated independently from one another. Updates to one should not
/// affect others, unless they contain overlapping selectors.
#[fuchsia::test]
async fn two_tags() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/two_tags.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(100i64)).await;
    realm.increment_integer_counter(1).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    // Wait at least one sample period. integer_1 should be updated to 11.
    zx::MonotonicDuration::from_seconds(2).sleep();
    realm.increment_integer_counter(2).await;
    realm.increment_integer_counter(2).await;

    // Wait at least another sample period. integer_2 should be updated to be 12.
    zx::MonotonicDuration::from_seconds(2).sleep();

    // Persistence should publish the Inspect state.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;

    let realm_name = realm.instance.root.child_name();
    realm
        .verify_diagnostics_persistence_publication(Published::Content(format!(
            r#"
        "{SERVICE}": {{
            "test-component-metric-a": {{
                "@errors": [],
                "@persist_size": 43,
                "@timestamps": {{
                    "last_sample_boot": 0,
                    "last_sample_utc": 0
                }},
                "realm_builder:{realm_name}/single_counter": {{
                    "samples": {{
                        "optional": 100,
                        "integer_1": 11
                    }}
                }}
            }},
            "test-component-metric-b": {{
                "@errors": [],
                "@persist_size": 43,
                "@timestamps": {{
                    "last_sample_boot": 0,
                    "last_sample_utc": 0
                }},
                "realm_builder:{realm_name}/single_counter": {{
                    "samples": {{
                        "optional": 100,
                        "integer_2": 22
                    }}
                }}
            }}
        }}
    "#
        )))
        .await;
}

/// Configuring `min_seconds_between_fetch` should limit Persistence from
/// persisting new Inspect state too often.
#[fuchsia::test]
async fn waits_sample_period() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/never_fetch.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    // Persistence should NOT fetch the new state immediately; it should still
    // be waiting for `min_seconds_between_fetch` to elapse.
    realm.set_inspect(Some(20i64)).await;
    realm.verify_diagnostics_persistence_publication(Published::Waiting).await;

    // Persistence should publish the older Inspect state.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;
}

/// Persisted data shouldn't be published until Persistence is killed and
/// restarted, and the update has completed.
#[fuchsia::test]
async fn waits_to_publish_data() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")))
            .await;

    // Persistence should fetch Inspect state immediately after starting.
    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;
    zx::MonotonicDuration::from_seconds(2).sleep(); // Wait at least 1 sample period.
    realm.verify_diagnostics_persistence_publication(Published::Waiting).await;

    // Persistence should not publish without the update check.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;
    zx::MonotonicDuration::from_seconds(2).sleep(); // Wait at least 1 sample period.
    realm.verify_diagnostics_persistence_publication(Published::Waiting).await;

    // A successful update check will trigger publishing of persisted data.
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;

    // After another restart, data for tags without "persist_across_boot"
    // shouldn't be published before nor after update is completed.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Default).await;
}

/// When `skip_update_check` is enabled, Persistence shouldn't wait for a
/// successful update check before publishing.
#[fuchsia::test]
async fn skip_update_check() {
    let realm = TestRealm::new(
        TestRealmOptions::new(include_str!("test_data/config/single_tag.persist"))
            .skip_update_check(),
    )
    .await;

    realm.set_inspect(Some(19i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;

    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;
}

/// Persisting data larger than max_bytes should abort the operation, instead
/// inserting an error into the persisted data.
#[fuchsia::test]
async fn too_big() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/too_big.persist")))
            .await;

    realm.set_inspect(Some(9i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;
    zx::MonotonicDuration::from_seconds(2).sleep(); // Wait at least 1 sample period.

    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::SizeError(9i64)).await;
}

/// Tags with persist_across_boot should remain after restart.
#[fuchsia::test]
async fn persist_across_boot() {
    let realm = TestRealm::new(TestRealmOptions::new(include_str!(
        "test_data/config/persist_across_boot.persist"
    )))
    .await;

    // Set the Inspect field to a custom value. This value won't publish till
    // the next reboot.
    realm.set_inspect(Some(8i64)).await;
    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_disk_write().await;
    realm.verify_diagnostics_persistence_publication(Published::Waiting).await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Empty).await;

    // After the first reboot, we should see the custom value. It should also
    // persist this value to the current persisted data.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(8)).await;

    // The next boot should continue to have the custom value since this tag is
    // marked with "persist_across_reboot".
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(8)).await;

    // Once more for good measure.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Int(8)).await;
}

/// Verify Persistence starts and never stops when StopOnIdleTimeoutsMillis is
/// negative.
#[fuchsia::test]
async fn never_idles() {
    let realm =
        TestRealm::new(TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")))
            .await;
    let mut event_stream = EventStream::open().await.unwrap().fuse();
    realm.start_persistence().await;
    realm.wait_for_event::<Started>(&mut event_stream).await;

    // Allow Persistence to publish to Inspect.
    realm.wait_for_disk_write().await;
    realm.verify_diagnostics_persistence_publication(Published::Waiting).await;
    realm.set_update_completed().await;
    realm.verify_diagnostics_persistence_publication(Published::Empty).await;

    assert_matches!(
        realm
            .listen_for_event::<Stopped>(&mut event_stream, MonotonicDuration::from_seconds(5))
            .await,
        Ok(None),
        "Unexpectedly received Stopped lifecycle event from the Persistence component"
    );
}

/// Verify Persistence starts and always idles when StopOnIdleTimeoutMillis is
/// zero.
#[fuchsia::test]
async fn always_idles() {
    let mut event_stream = EventStream::open().await.unwrap().fuse();
    let realm = TestRealm::new(
        TestRealmOptions::new(include_str!("test_data/config/single_tag.persist")).with_idle(0),
    )
    .await;

    wait_for_inspect_source(&realm.instance).await;
    realm.start_persistence().await;
    realm.wait_for_stopped_steady_state(&mut event_stream).await;

    // Changing subscribed Inspect data will cause the component to start then
    // stop again.
    realm.set_inspect(Some(19i64)).await;
    realm.wait_for_stopped_steady_state(&mut event_stream).await;

    // Persistence should publish the Inspect state on the next boot.
    let realm = realm.restart().await;
    realm.start_persistence().await;
    realm.set_update_completed().await;

    // Wait for the absence of a start event; ensures the Inspect VMO is escrowed.
    realm.wait_for_stopped_steady_state(&mut event_stream).await;
    realm.verify_diagnostics_persistence_publication(Published::Int(19)).await;
}

/// The Inspect source may not publish Inspect (via take_and_serve_directory_handle()) until
/// some time after the FIDL call that woke it up has returned. This function verifies that
/// the Inspect source is actually publishing data to avoid a race condition.
async fn wait_for_inspect_source(realm: &RealmInstance) {
    let accessor_proxy = realm
        .root
        .connect_to_protocol_at_exposed_dir()
        .expect("Failed to connect to ArchiveAccessor");
    let mut inspect_fetcher = ArchiveReader::inspect();
    inspect_fetcher
        .with_archive(accessor_proxy)
        .retry(RetryConfig::never())
        .add_selector("realm_builder*/single_counter:root");
    let start_time = MonotonicInstant::get();

    loop {
        assert!(
            start_time + MonotonicDuration::from_seconds(GIVE_UP_POLLING_SECS)
                > MonotonicInstant::get()
        );
        let published_inspect =
            inspect_fetcher.snapshot_raw::<serde_json::Value>().await.unwrap().to_string();
        if published_inspect.contains(KEY_FROM_INSPECT_SOURCE) {
            return;
        }
        fasync::Timer::new(MonotonicDuration::from_millis(100)).await;
    }
}

pub(crate) struct TestRealmOptions {
    name: String,
    config: &'static str,
    filesystem: mock_filesystems::TestFs,
    skip_update_check: bool,
    stop_on_idle_timeout_millis: i64,
}

impl TestRealmOptions {
    fn new(config: &'static str) -> Self {
        // Generate a unique name for each test realm to prevent conflicts
        // between test runs.
        let name = {
            let id: u64 = rand::random();
            format!("auto-{id:x}")
        };
        Self {
            name,
            config,
            filesystem: mock_filesystems::TestFs::new(),
            skip_update_check: false,
            stop_on_idle_timeout_millis: -1,
        }
    }
    fn skip_update_check(self) -> Self {
        Self { skip_update_check: true, ..self }
    }
    fn with_idle(self, timeout_millis: i64) -> Self {
        Self { stop_on_idle_timeout_millis: timeout_millis, ..self }
    }
}

impl TestRealm {
    async fn new(options: TestRealmOptions) -> Self {
        let instance = test_topology::create(&options).await;
        // `inspect` is the source of Inspect data that Persistence will read and persist.
        let inspect = instance.root.connect_to_protocol_at_exposed_dir().unwrap();
        // `controller` is the connection to send control signals to the test's update-checker mock.
        let controller = instance.root.connect_to_protocol_at_exposed_dir().unwrap();
        TestRealm { options, instance, inspect, controller }
    }

    async fn start_persistence(&self) {
        // Start up the Persistence component by sending a request to one of its
        // served FIDLs.
        let _persistence_binder = self
            .instance
            .root
            .connect_to_named_protocol_at_exposed_dir::<BinderMarker>(
                "fuchsia.component.PersistenceBinder",
            )
            .unwrap();

        // These test are single threaded and will deadlock while loading
        // Persistence configs unless sleeps are added to allow
        // config-data-server to serve requests. This could also be fixed by
        // setting threads=2 in the fuchsia_test macro.
        if self.options.skip_update_check {
            let inspector = self.archive_reader_with_selector(format!(
                "realm_builder\\:{}/persistence:root/fuchsia.inspect.Health:status",
                self.instance.root.child_name()
            ));

            loop {
                fasync::Timer::new(MonotonicDuration::from_millis(100)).await;
                let snapshot = inspector.snapshot().await.unwrap();
                if let Some(hierarchy) = snapshot.into_iter().next().and_then(|d| d.payload)
                    && let Some(status) = hierarchy
                        .get_property_by_path(&["fuchsia.inspect.Health", "status"])
                        .and_then(|p| p.string())
                    && status == "STARTING_UP"
                {
                    break;
                }
            }
        } else {
            self.verify_diagnostics_persistence_publication(Published::Waiting).await;
        }
    }

    // Wait for Persistence to finish writing current data to disk.
    async fn wait_for_disk_write(&self) {
        let current_data_path = format!("{}/current.json", self.options.filesystem.cache());
        loop {
            match fuchsia_fs::file::read_in_namespace(&current_data_path).await {
                Ok(_) => break,
                Err(e) if e.is_not_found_error() => {
                    fuchsia_async::Timer::new(fuchsia_async::MonotonicDuration::from_millis(100))
                        .await;
                }
                Err(e) => panic!(
                    "Unexpectedly failed to read current data \"{}\": {e:?}",
                    current_data_path
                ),
            }
        }
    }

    async fn set_update_completed(&self) {
        self.controller.set_update_completed().await.expect("This should never fail");
    }

    /// Set the `optional` value to a given number, or remove it from the Inspect tree.
    async fn set_inspect(&self, value: Option<i64>) {
        match value {
            Some(value) => {
                self.inspect.set_optional(value).await.expect("set_optional should work")
            }
            None => self.inspect.remove_optional().await.expect("remove_optional should work"),
        };
    }

    /// Increment an "integer_*" counter in single-counter-test-component by ID.
    ///
    /// The test component contains 3 integer properties:
    ///  - integer_1
    ///  - integer_2
    ///  - integer_3
    async fn increment_integer_counter(&self, counter: u16) {
        // Integer properties are included into the map at a +1 offset.
        self.inspect.increment_int(counter + 1).await.expect("Incrementing counter failed")
    }

    /// Tear down the realm to make sure everything is gone before you restart it.
    /// Then create and return a new realm.
    async fn restart(self) -> TestRealm {
        let Self { options, instance, inspect: _inspect, controller: _controller } = self;
        instance.destroy().await.expect("destroy should work");
        TestRealm::new(options).await
    }

    /// Create an ArchiveReader scoped to this test realm.
    fn archive_reader_with_selector(
        &self,
        selector: impl ToSelectorArguments,
    ) -> ArchiveReader<Inspect> {
        let accessor_proxy = self
            .instance
            .root
            .connect_to_protocol_at_exposed_dir()
            .expect("Failed to connect to ArchiveAccessor");
        let mut reader = ArchiveReader::inspect();
        reader.with_archive(accessor_proxy).retry(RetryConfig::never()).add_selector(selector);
        reader
    }

    /// Verify that the expected data is published by Persistence in its Inspect hierarchy.
    async fn verify_diagnostics_persistence_publication(&self, published: Published) {
        let inspect_fetcher = self.archive_reader_with_selector(format!(
            "realm_builder\\:{}/persistence:root",
            self.instance.root.child_name()
        ));

        loop {
            fasync::Timer::new(MonotonicDuration::from_millis(100)).await;
            let published_inspect =
                inspect_fetcher.snapshot_raw::<serde_json::Value>().await.unwrap();
            let published_inspect = serde_json::to_string_pretty(&published_inspect).unwrap();
            if matches!(published, Published::Waiting) && published_inspect.contains("STARTING_UP")
            {
                break;
            } else if published_inspect.contains(PUBLISHED_TIME_KEY) {
                assert!(json_strings_match(
                    &clean_component_url(unbrittle_too_big_message(zero_and_test_timestamps(
                        &published_inspect
                    ))),
                    &expected_diagnostics_persistence_inspect(
                        self.instance.root.child_name(),
                        &self.options,
                        published
                    ),
                    "persistence publication"
                ));
                break;
            }
        }
    }

    /// Listen for the Persistence component to emit the specified lifecycle
    /// event within the specified duration. Returns None if timeout elapsed
    /// without finding a matching event.
    async fn listen_for_event<T: Event>(
        &self,
        event_stream: &mut Fuse<EventStream>,
        duration: impl fasync::WakeupTime,
    ) -> Result<Option<T>, anyhow::Error> {
        let timeout = pin!(fasync::Timer::new(duration).fuse());
        self.listen_for_event_with_timeout(event_stream, timeout).await
    }

    /// Listen for the Persistence component to emit the specified lifecycle
    /// event. Returns None if timeout elapsed without finding a matching event.
    async fn listen_for_event_with_timeout<T: Event>(
        &self,
        event_stream: &mut Fuse<EventStream>,
        mut timeout: Pin<&mut futures::future::Fuse<fasync::Timer>>,
    ) -> Result<Option<T>, anyhow::Error> {
        let moniker = format!(".*{}.*persistence$", self.instance.root.child_name());
        let matcher = EventMatcher::ok().moniker_regex(moniker).r#type(T::TYPE);
        loop {
            select! {
                event = event_stream.next() => {
                    let event = event.ok_or(EventStreamError::StreamClosed)?;
                    let descriptor = EventDescriptor::try_from(&event)?;
                    if let Ok(()) = matcher.matches(&descriptor) {
                        return T::try_from(event).map(Some);
                    }
                }
                () = timeout => {
                    return Ok(None);
                },
            }
        }
    }

    /// Wait indefinitely for the Persistence component to emit the specified
    /// lifecycle event.
    async fn wait_for_event<T: Event>(&self, event_stream: &mut Fuse<EventStream>) {
        let moniker = format!(".*{}.*persistence$", self.instance.root.child_name());
        let matcher = EventMatcher::ok().moniker_regex(moniker).r#type(T::TYPE);
        loop {
            let event = event_stream.next().await.unwrap();
            let descriptor = EventDescriptor::try_from(&event).unwrap();
            if let Ok(()) = matcher.matches(&descriptor) {
                return;
            }
        }
    }

    /// Wait for the Persistent component in this realm to steady state in
    /// Stopped by debouncing start events within a set duration.
    async fn wait_for_stopped_steady_state(&self, event_stream: &mut Fuse<EventStream>) {
        let mut start_count = 0;
        loop {
            let timeout = pin!(fasync::Timer::new(MonotonicDuration::from_seconds(2)).fuse());

            if self
                .listen_for_event_with_timeout::<Started>(event_stream, timeout)
                .await
                .unwrap()
                .is_none()
            {
                // No start event came within 2 seconds; unlikely to start again.
                debug!("wait_for_stopped_steady_state debounced {start_count} starts");
                return;
            }

            start_count += 1;
            self.wait_for_event::<Stopped>(event_stream).await;
        }
    }
}

/// Given a mut map from a JSON object that's presumably sourced from Inspect, if it contains a
/// timestamp record entry, this function validates fields exist and zeros them.
fn clean_and_test_timestamps(map: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Object(map)) = map.get_mut(TIMESTAMP_STRUCT_KEY) {
        for key in TIMESTAMP_STRUCT_ENTRIES.iter() {
            assert_matches!(
                map.insert(key.to_string(), serde_json::json!(0)),
                Some(Value::Number(_))
            );
        }
    }
}

/// The number of bytes reported in the "too big" case may vary. It should be a 2-digit
/// number. Replace with underscores.
fn unbrittle_too_big_message(contents: String) -> String {
    let matcher = regex::Regex::new(r"Data too big: \d{2} > max length 10").unwrap();
    matcher.replace_all(&contents, "Data too big: __ > max length 10").to_string()
}

/// Remove index in component_url.
fn clean_component_url(contents: String) -> String {
    let matcher = regex::Regex::new(r"realm-builder://\d+/persistence").unwrap();
    matcher.replace_all(&contents, "realm-builder/persistence").to_string()
}

fn json_strings_match(observed: &str, expected: &str, context: &str) -> bool {
    let mut observed_json: Value = serde_json::from_str(observed)
        .unwrap_or_else(|e| panic!("Error parsing observed json in {context}: {e:?}"));

    // Remove health nodes if they exist.
    if let Some(v) = observed_json.as_array_mut() {
        for hierarchy in v.iter_mut() {
            if let Some(Some(root)) =
                hierarchy.pointer_mut("/payload/root").map(|r| r.as_object_mut())
            {
                root.remove("fuchsia.inspect.Health");
            }
        }
    }

    let expected_json: Value = serde_json::from_str(expected).unwrap_or_else(|e| {
        panic!("Error parsing expected json in {context}: {e:?}, data: {expected}")
    });

    if observed_json != expected_json {
        let observed = serde_json::to_string_pretty(&observed_json).unwrap();
        let expected = serde_json::to_string_pretty(&expected_json).unwrap();
        warn!("Observed != expected \n{}", StrComparison::new(&observed, &expected));
    }
    observed_json == expected_json
}

fn zero_and_test_timestamps(contents: &str) -> String {
    fn for_all_entries<F>(map: &mut serde_json::Map<String, Value>, func: F)
    where
        F: Fn(&mut serde_json::Map<String, Value>),
    {
        for (_key, value) in map.iter_mut() {
            if let Value::Object(inner_map) = value {
                func(inner_map);
            }
        }
    }

    let result_json: Value = serde_json::from_str(contents).expect("parsing json failed.");
    let mut string_result_array = result_json
        .as_array()
        .expect("result json is an array of objs.")
        .iter()
        .filter_map(|val| {
            let mut val = val.clone();

            val.as_object_mut().map(|obj: &mut serde_json::Map<String, serde_json::Value>| {
                let metadata_obj = obj.get_mut(METADATA_KEY).unwrap().as_object_mut().unwrap();
                metadata_obj.insert(TIMESTAMP_METADATA_KEY.to_string(), serde_json::json!(0));
                let payload_obj = obj.get_mut(PAYLOAD_KEY).unwrap();
                if let Value::Object(map) = payload_obj
                    && let Some(Value::Object(map)) = map.get_mut(ROOT_KEY)
                {
                    if map.contains_key(PUBLISHED_TIME_KEY) {
                        map.insert(PUBLISHED_TIME_KEY.to_string(), serde_json::json!(0));
                    }
                    if let Some(Value::Object(persist_contents)) = map.get_mut(PERSIST_KEY) {
                        for_all_entries(persist_contents, |service_contents| {
                            for_all_entries(service_contents, clean_and_test_timestamps);
                        });
                    }
                }
                serde_json::to_string_pretty(&serde_json::to_value(obj).unwrap())
                    .expect("All entries in the array are valid.")
            })
        })
        .collect::<Vec<String>>();

    string_result_array.sort();

    format!("[{}]", string_result_array.join(","))
}

fn expected_diagnostics_persistence_inspect(
    realm_name: &str,
    options: &TestRealmOptions,
    published: Published,
) -> String {
    let content = match published {
        Published::Waiting | Published::Empty | Published::Content(_) => "".to_string(),
        Published::Default => r#"
            {
                "samples": {
                    "integer_1": 10
                }
            }
        "#
        .to_string(),
        Published::SizeError(number) => format!(
            r#"
                {{
                    "samples": {{
                        "optional": {number},
                        "integer_1": 10
                    }}
                }}
            "#
        ),
        Published::Int(number) => format!(
            r#"
                {{
                    "samples": {{
                        "optional": {number},
                        "integer_1": 10
                    }}
                }}
            "#
        ),
    };

    let persist_size = if content.is_empty() {
        0
    } else {
        let value = serde_json::from_str::<serde_json::Value>(&content).unwrap();
        let content = serde_json::to_string(&value).unwrap();
        content.len()
    };

    let TestRealmOptions { skip_update_check, stop_on_idle_timeout_millis, .. } = options;

    let config = format!(
        r#"
            "config": {{
                "skip_update_check": {skip_update_check},
                "stop_on_idle_timeout_millis": {stop_on_idle_timeout_millis}
            }}
        "#
    );

    let variant = match published {
        Published::Waiting => format!(
            r#"
                {config}
            "#
        ),
        Published::Empty => format!(
            r#"
                {config},
                "published": 0
            "#
        ),
        Published::Content(content) => format!(
            r#"
                {config},
                "persist": {{
                    {content}
                }},
                "published": 0
            "#
        ),
        Published::Default | Published::Int(_) => format!(
            r#"
                {config},
                "persist": {{
                    "{SERVICE}": {{
                        "{TAG}": {{
                            "@errors": [],
                            "@persist_size": {persist_size},
                            "@timestamps": {{
                                "last_sample_boot": 0,
                                "last_sample_utc": 0
                            }},
                            "realm_builder:{realm_name}/single_counter": {content}
                        }}
                    }}
                }},
                "published": 0
            "#
        ),
        // unbrittle_too_big_message() will replace a 2-digit number after
        // "big: " with "__"
        Published::SizeError(_) => format!(
            r#"
                {config},
                "persist": {{
                    "{SERVICE}": {{
                        "{TAG}": {{
                            "@errors": [
                                "Data too big: __ > max length 10"
                            ],
                            "@persist_size": {persist_size},
                            "@timestamps": {{
                                "last_sample_boot": 0,
                                "last_sample_utc": 0
                            }}
                        }}
                    }}
                }},
                "published": 0
            "#
        ),
    };

    let escrowed = if options.stop_on_idle_timeout_millis < 0 {
        ""
    } else {
        r#"
            "escrowed": true,
        "#
    };

    format!(
        r#"
        [
            {{
                "data_source": "Inspect",
                "metadata": {{
                    "component_url": "realm-builder/persistence",
                    {escrowed}
                    "name": "root",
                    "timestamp": 0
                }},
                "moniker": "realm_builder:{realm_name}/persistence",
                "payload": {{
                    "root": {{
                        {variant}
                    }}
                }},
                "version": 1
            }}
        ]
    "#
    )
}
