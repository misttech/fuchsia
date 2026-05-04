// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use fidl_fuchsia_update_channel as fupdate_channel;
use fuchsia_async::{self as fasync};
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use test_case::test_case;

const NULL_UPDATE_CHECKER_CM: &str = "#meta/null-update-checker.cm";

struct TestEnvBuilder {
    current_ota_channel: Option<String>,
    idle_timeout_millis: Option<i64>,
}
impl TestEnvBuilder {
    fn new() -> Self {
        Self { current_ota_channel: None, idle_timeout_millis: None }
    }

    fn current_ota_channel(mut self, current_ota_channel: impl Into<String>) -> Self {
        assert_eq!(self.current_ota_channel, None);
        self.current_ota_channel = Some(current_ota_channel.into());
        self
    }

    fn idle_timeout_millis(mut self, idle_timeout_millis: i64) -> Self {
        assert_eq!(self.idle_timeout_millis, None);
        self.idle_timeout_millis = Some(idle_timeout_millis);
        self
    }

    async fn build(self) -> TestEnv {
        let Self { current_ota_channel, idle_timeout_millis } = self;
        let builder = RealmBuilder::new().await.unwrap();
        let null_update_checker = builder
            .add_child("null-update-checker", NULL_UPDATE_CHECKER_CM, ChildOptions::new().eager())
            .await
            .unwrap();
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fidl_fuchsia_logger::LogSinkMarker>())
                    .from(Ref::parent())
                    .to(&null_update_checker),
            )
            .await
            .unwrap();
        for (config_name, value) in [
            ("fuchsia.ota_channel", current_ota_channel.unwrap_or_default().into()),
            (
                "fuchsia.null-update-checker.StopOnIdleTimeoutMillis",
                idle_timeout_millis.unwrap_or(-1i64).into(),
            ),
        ] {
            builder
                .add_capability(
                    cm_rust::ConfigurationDecl { name: config_name.parse().unwrap(), value }.into(),
                )
                .await
                .unwrap();
            builder
                .add_route(
                    Route::new()
                        .capability(Capability::configuration(config_name))
                        .from(Ref::self_())
                        .to(&null_update_checker),
                )
                .await
                .unwrap();
        }
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fupdate_channel::ProviderMarker>())
                    .from(&null_update_checker)
                    .to(Ref::parent()),
            )
            .await
            .unwrap();

        let realm_instance = builder.build().await.unwrap();
        let channel_provider = realm_instance
            .root
            .connect_to_protocol_at_exposed_dir()
            .expect("connect to commit status provider");

        TestEnv { realm_instance, channel_provider }
    }
}
struct TestEnv {
    realm_instance: RealmInstance,
    channel_provider: fupdate_channel::ProviderProxy,
}

impl TestEnv {
    fn builder() -> TestEnvBuilder {
        TestEnvBuilder::new()
    }

    async fn wait_for_started(&self, event_stream: &mut component_events::events::EventStream) {
        component_events::matcher::EventMatcher::ok()
            .moniker_regex(format!(
                "^realm_builder:{}/null-update-checker$",
                self.realm_instance.root.child_name()
            ))
            .wait::<component_events::events::Started>(event_stream)
            .await
            .unwrap();
    }

    async fn wait_for_clean_stopped(
        &self,
        event_stream: &mut component_events::events::EventStream,
    ) {
        let stopped = component_events::matcher::EventMatcher::ok()
            .moniker_regex(format!(
                "^realm_builder:{}/null-update-checker$",
                self.realm_instance.root.child_name()
            ))
            .wait::<component_events::events::Stopped>(event_stream)
            .await
            .unwrap();
        assert_matches!(
            stopped.result().unwrap(),
            component_events::events::StoppedPayload {
                status: component_events::events::ExitStatus::Clean,
                exit_code: Some(0)
            }
        );
    }

    /// Obtains a new connection to fuchsia.update.channel/Provider.
    fn fresh_channel_provider_proxy(&self) -> fupdate_channel::ProviderProxy {
        self.realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap()
    }
}

#[test_case(-1i64; "never idle")]
#[test_case(0i64; "rapid idle")]
#[fasync::run_singlethreaded(test)]
async fn query_current_channel(idle_timeout_millis: i64) {
    let env = TestEnv::builder()
        .idle_timeout_millis(idle_timeout_millis)
        .current_ota_channel("injected-by-test")
        .build()
        .await;

    assert_eq!(env.channel_provider.get_current().await.unwrap(), "injected-by-test");
}

// If configured, when the null-update-checker is idle (when there has not been any activity on
// its out dir or any outstanding fidl connections for a period of time), the null-update-checker
// escrows its state with the CM and stops itself. Later, when there is activity again, CM restarts
// the null-update-checker which should then retrieve the escrowed state and handle the activity
// until it is time to idle-stop again.
// This tests that the null-update-checker stops when idle and correctly resumes from its escrowed
// state, which includes verifying that:
// 1. activity on connections that existed when the component stopped itself will restart the
//    component and be handled correctly
// 2. activity on the out dir while the component is stopped will restart the component and be
//    handled correctly
#[fasync::run_singlethreaded(test)]
async fn stop_on_idle_resume_on_use() {
    let mut event_stream = component_events::events::EventStream::open().await.unwrap();
    let env =
        TestEnv::builder().idle_timeout_millis(0).current_ota_channel("threeve").build().await;

    // A new message on a channel should start the component.
    assert_eq!(env.channel_provider.get_current().await.unwrap(), "threeve");
    env.wait_for_started(&mut event_stream).await;

    // The component should stop when the timeout is hit even though there is an open connection.
    env.wait_for_clean_stopped(&mut event_stream).await;

    // Using the open connection should start the component.
    assert_eq!(env.channel_provider.get_current().await.unwrap(), "threeve");
    env.wait_for_started(&mut event_stream).await;

    // Should still be able to stop.
    env.wait_for_clean_stopped(&mut event_stream).await;

    // A new connection should start the component.
    let new_proxy = env.fresh_channel_provider_proxy();
    assert_eq!(new_proxy.get_current().await.unwrap(), "threeve");
    env.wait_for_started(&mut event_stream).await;
    env.wait_for_clean_stopped(&mut event_stream).await;

    // The new connection should also support escrow.
    assert_eq!(new_proxy.get_current().await.unwrap(), "threeve");
    env.wait_for_started(&mut event_stream).await;
    env.wait_for_clean_stopped(&mut event_stream).await;

    // The old connection should still work.
    assert_eq!(env.channel_provider.get_current().await.unwrap(), "threeve");
    env.wait_for_started(&mut event_stream).await;
    env.wait_for_clean_stopped(&mut event_stream).await;
}
