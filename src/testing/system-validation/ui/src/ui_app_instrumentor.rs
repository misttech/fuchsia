// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::FromArgs;
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_session_scene as scene;
use fidl_fuchsia_ui_app as ui_app;
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_scenic as scenic;
use log::info;
use system_validation_lib::app_monitor::AppMonitor;
use system_validation_lib::screencapture::take_screenshot;
use system_validation_lib::single_session_trace::SingleSessionTrace;

#[derive(Default, PartialEq, Debug, Clone)]
struct CommaSeparated(Vec<String>);

/// Args to control how to run system validation test.
/// Specify them through *_system_validation.cml
#[derive(FromArgs, PartialEq, Debug)]
struct RunTestCmd {
    /// trace configurations, input is a comma delimited string
    /// example: --trace-config "system_metrics,input,gfx"
    #[argh(option, default = "CommaSeparated::default()", from_str_fn(to_vector))]
    trace_config: CommaSeparated,

    /// number of seconds to run example app, default is 5 seconds
    #[argh(
        option,
        default = "fasync::MonotonicDuration::from_seconds(5)",
        from_str_fn(to_duration)
    )]
    run_duration_sec: fasync::MonotonicDuration,
}

// Note: Maybe we want to make this an argument.
const SAMPLE_APP_MONIKER: &str = "./sample-app";

#[fuchsia::main]
async fn main() {
    let args: RunTestCmd = argh::from_env();
    // Open the component EventStream before connecting to ViewProvider or Binder so `#sample-app`'s
    // `Started` event is not missed.
    let app_monitor = AppMonitor::new(SAMPLE_APP_MONIKER.to_string()).await;

    // The test manifest determines which launch path to take:
    // If the test manifest routes `fuchsia.ui.app.ViewProvider` from `#sample-app` into our namespace,
    // we connect to ViewProvider and have scene_manager present the root view.
    // Otherwise, for self-presenting apps (which use GraphicalPresenter), the test manifest offers
    // GraphicalPresenter to `#sample-app` and uses `fuchsia.component.Binder` from `#sample-app`,
    // and connecting to `Binder` triggers `component_manager` to start `#sample-app`.
    let view_provider_path = format!("/svc/{}", ui_app::ViewProviderMarker::PROTOCOL_NAME);
    if std::path::Path::new(&view_provider_path).exists() {
        // Hook up to scene_manager for apps that expose ViewProvider.
        let scene_manager = connect_to_protocol::<scene::ManagerMarker>()
            .expect("failed to connect to fuchsia.scene.Manager");
        let view_provider = connect_to_protocol::<ui_app::ViewProviderMarker>()
            .expect("failed to connect to ViewProvider");
        let link_token_pair = scenic::flatland::ViewCreationTokenPair::new().unwrap();

        // Use view provider to initiate creation of the view which will be connected to the
        // viewport that we create below.
        view_provider
            .create_view2(ui_app::CreateView2Args {
                view_creation_token: Some(link_token_pair.view_creation_token),
                ..Default::default()
            })
            .expect("Cannot invoke create_view2");
        scene_manager
            .present_root_view(link_token_pair.viewport_creation_token)
            .await
            .expect("failed to call present_root_view")
            .expect("PresentRootView returned error");
    } else {
        // Connecting to Binder triggers `component_manager` to start `#sample-app`,
        // which then presents itself via GraphicalPresenter.
        let _ = connect_to_protocol::<fcomponent::BinderMarker>()
            .expect("failed to connect to fuchsia.component.Binder");
    }
    app_monitor.wait_for_start_event().await;
    app_monitor.add_monitor_for_stop_event();

    // Collect trace.
    let trace = SingleSessionTrace::new();
    let trace_config = args.trace_config.0;
    let collect_trace = !trace_config.is_empty();
    if collect_trace {
        info!("Collecting trace: {:?}", trace_config);
        trace.initialize(trace_config).await.unwrap();
        trace.start().await.unwrap();
    }
    // Let the UI App run for [run_duration_sec], then the test_runner will shutdown the sample_app
    let duration_sec = args.run_duration_sec;
    info!("Running sample app for {:?} sec", duration_sec);
    fasync::Timer::new(duration_sec).await;

    if collect_trace {
        trace.stop().await.unwrap();
        trace.terminate().await.unwrap();
    }

    info!("Taking screenshot.");
    take_screenshot().await;

    info!("Checking that sample-app did not crash.");
    if app_monitor.has_seen_stop_event() {
        panic!("Sample app unexpectedly stopped");
    }
}

fn to_duration(duration_sec: &str) -> Result<fasync::MonotonicDuration, String> {
    Ok(fasync::MonotonicDuration::from_seconds(duration_sec.parse::<i64>().unwrap()))
}

fn to_vector(configs: &str) -> Result<CommaSeparated, String> {
    Ok(CommaSeparated(configs.split(',').map(|v| v.trim().to_string()).collect()))
}
