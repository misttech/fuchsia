// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_samplertestcontroller::*;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_sync::Mutex;
use futures::{FutureExt, StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::sync::Arc;

const OBSERVED_ID: &str = "1111222233334444111111111111111111111111111111111111111111111111";
const IGNORED_ID: &str = "2222222233334444111111111111111111111111111111111111111111112222";

// Additional named Inspect trees published alongside the default "root" tree. These are used to
// verify that tree-name-filtered selectors (e.g. `single_counter:[name=target_tree]root:...`) are
// sampled from exactly the tree they name, even when other trees in the same snapshot lack the
// selected property.
const TARGET_TREE_NAME: &str = "target_tree";
const UNRELATED_TREE_1_NAME: &str = "unrelated_1";
const UNRELATED_TREE_2_NAME: &str = "unrelated_2";

#[fuchsia::main]
async fn main() {
    let state = TestState::new();

    let inspector = component::inspector();
    let _lazy_state_node = add_lazy_sampled_node(inspector.root(), state.clone(), true);
    let _inspect_server_task =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default());

    let target_tree_inspector = fuchsia_inspect::Inspector::default();
    let _lazy_target_samples_node =
        add_lazy_sampled_node(target_tree_inspector.root(), state.clone(), false);
    let _lazy_target_only_node =
        add_lazy_target_only_node(target_tree_inspector.root(), state.clone());
    let _target_tree_server_task = inspect_runtime::publish(
        &target_tree_inspector,
        inspect_runtime::PublishOptions::default().inspect_tree_name(TARGET_TREE_NAME),
    );

    let unrelated_1_inspector = fuchsia_inspect::Inspector::default();
    unrelated_1_inspector.root().record_child("unrelated", |node| node.record_int("val", 0));
    let _unrelated_1_server_task = inspect_runtime::publish(
        &unrelated_1_inspector,
        inspect_runtime::PublishOptions::default().inspect_tree_name(UNRELATED_TREE_1_NAME),
    );

    let unrelated_2_inspector = fuchsia_inspect::Inspector::default();
    unrelated_2_inspector.root().record_child("unrelated", |node| node.record_int("val", 0));
    let _unrelated_2_server_task = inspect_runtime::publish(
        &unrelated_2_inspector,
        inspect_runtime::PublishOptions::default().inspect_tree_name(UNRELATED_TREE_2_NAME),
    );

    let mut fs = ServiceFs::new();
    log::info!("Started SamplerTestController");
    fs.dir("svc")
        .add_fidl_service(move |stream| serve_sampler_test_controller(stream, state.clone()));

    fs.take_and_serve_directory_handle().unwrap();

    fs.collect::<()>().await
}

struct TestState {
    integer_property_map: HashMap<u16, (String, i64)>,
    optional_integer: Option<i64>,
    sample_count_callback_opt: Option<SamplerTestControllerWaitForSampleResponder>,
}

static INSPECT_NAME_FOR_OPTIONAL: &str = "optional";
impl TestState {
    pub fn new() -> Arc<Mutex<TestState>> {
        let mut integer_property_map: HashMap<u16, (String, i64)> = HashMap::new();
        integer_property_map.insert(1, ("counter".to_string(), 0));
        integer_property_map.insert(2, ("integer_1".to_string(), 10));
        integer_property_map.insert(3, ("integer_2".to_string(), 20));
        integer_property_map.insert(4, ("integer_3".to_string(), 30));
        integer_property_map.insert(5, (OBSERVED_ID.to_string(), 42));
        integer_property_map.insert(6, (IGNORED_ID.to_string(), 17));
        Arc::new(Mutex::new(TestState {
            integer_property_map,
            optional_integer: None,
            sample_count_callback_opt: None,
        }))
    }
}

/// Creates a lazy "samples" child under `parent` exposing the current test state.
///
/// When `notify_sample` is true, reading this node completes any pending `WaitForSample` call.
/// Only one lazy node should set `notify_sample` so that a single snapshot reading multiple trees
/// never completes more than one callback.
fn add_lazy_sampled_node(
    parent: &fuchsia_inspect::Node,
    state: Arc<Mutex<TestState>>,
    notify_sample: bool,
) -> fuchsia_inspect::LazyNode {
    parent.create_lazy_child("samples", move || {
        let state = state.clone();
        async move {
            let mut unwrapped = state.lock();

            if notify_sample && let Some(responder) = unwrapped.sample_count_callback_opt.take() {
                responder.send(Ok(())).unwrap();
            }

            let samples = fuchsia_inspect::Inspector::default();
            for (_, (name, int_val)) in unwrapped.integer_property_map.clone() {
                let () = samples.root().record_int(name, int_val);
            }
            if let Some(value) = unwrapped.optional_integer {
                let () = samples.root().record_int(INSPECT_NAME_FOR_OPTIONAL, value);
            }
            Ok(samples)
        }
        .boxed()
    })
}

/// Creates a lazy "target_only" child under `parent` exposing the current value of the counter.
///
/// This is only attached to the target tree, so `root/target_only:counter` is absent from every
/// other tree published by this component.
fn add_lazy_target_only_node(
    parent: &fuchsia_inspect::Node,
    state: Arc<Mutex<TestState>>,
) -> fuchsia_inspect::LazyNode {
    parent.create_lazy_child("target_only", move || {
        let state = state.clone();
        async move {
            let counter = state.lock().integer_property_map.get(&1).unwrap().1;
            let target_only = fuchsia_inspect::Inspector::default();
            let () = target_only.root().record_int("counter", counter);
            Ok(target_only)
        }
        .boxed()
    })
}

fn serve_sampler_test_controller(
    mut stream: SamplerTestControllerRequestStream,
    state: Arc<Mutex<TestState>>,
) {
    fasync::Task::spawn(async move {
        while let Some(req) = stream.try_next().await.unwrap() {
            match req {
                SamplerTestControllerRequest::IncrementInt { property_id, responder } => {
                    let mut unwrapped = state.lock();
                    unwrapped.integer_property_map.get_mut(&property_id).unwrap().1 += 1;
                    responder.send().unwrap();
                }
                SamplerTestControllerRequest::SetOptional { value, responder } => {
                    let mut unwrapped = state.lock();
                    unwrapped.optional_integer = Some(value);
                    responder.send().unwrap();
                }
                SamplerTestControllerRequest::RemoveOptional { responder } => {
                    let mut unwrapped = state.lock();
                    unwrapped.optional_integer = None;
                    responder.send().unwrap();
                }
                SamplerTestControllerRequest::WaitForSample { responder } => {
                    let mut unwrapped = state.lock();

                    if unwrapped.sample_count_callback_opt.is_some() {
                        responder.send(Err(SamplingError::MultipleSampleCallbacksError)).unwrap();
                        continue;
                    }

                    unwrapped.sample_count_callback_opt = Some(responder);
                }
            }
        }
    })
    .detach();
}
