// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::component::TerminateCallback;
use futures::prelude::*;
use log::{error, warn};
use std::process;
use std::rc::Rc;
use {fidl_fuchsia_component_runner as frunner, fuchsia_async as fasync};

mod component;
mod error;
mod loader;
mod util;

#[fuchsia::main]
async fn main() {
    enum IncomingRequest {
        ComponentRunner(frunner::ComponentRunnerRequestStream),
    }

    let env = Rc::new(fdf_env::Environment::start(0).expect("start fdf environment"));
    // TODO(https://fxbug.dev/403545512): Govern the number of threads in the environment with a
    // constant, adding more than one thread if appropriate.
    let mut fs = fuchsia_component::server::ServiceFs::new();

    let scope = fasync::Scope::new();
    fs.dir("svc").add_fidl_service(IncomingRequest::ComponentRunner);
    fs.take_and_serve_directory_handle().expect("failed to serve outgoing directory");
    fs.for_each_concurrent(None, |request: IncomingRequest| {
        let env = env.clone();
        let scope = scope.clone();
        async move {
            match request {
                IncomingRequest::ComponentRunner(stream) => {
                    handle_component_runner(stream, &env, &scope).await
                }
            }
        }
    })
    .await;
}

async fn handle_component_runner(
    mut stream: frunner::ComponentRunnerRequestStream,
    env: &fdf_env::Environment,
    scope: &fasync::ScopeHandle,
) {
    while let Some(Ok(request)) = stream.next().await {
        match request {
            frunner::ComponentRunnerRequest::Start {
                start_info,
                controller,
                control_handle: _,
            } => {
                let url = start_info.resolved_url.clone().unwrap_or_else(|| "<no url>".into());
                let terminate_cb: TerminateCallback = Box::new(|url| {
                    // If the runner forcefully kills a synchronous component, there is not much we
                    // can do to safely terminate the component's thread without terminating the
                    // entire process. This means any other components running in the DSO runner
                    // will be killed as well. Hopefully we don't end up here and the component
                    // shutdowns down gracefully.
                    error!(url:%; "Sync component forcefully killed, terminating process");
                    process::exit(1);
                });

                if let Err(err) =
                    component::start(start_info, controller, env, scope, terminate_cb).await
                {
                    warn!(err:%, url:%; "failed to start component");
                }
            }
            frunner::ComponentRunnerRequest::_UnknownMethod { ordinal, .. } => {
                warn!(ordinal:%; "unknown ComponentRunner request");
            }
        }
    }
}
