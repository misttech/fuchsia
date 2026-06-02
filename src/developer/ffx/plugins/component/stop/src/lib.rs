// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use component_debug_fdomain::cli::stop_cmd;
use errors::ffx_error;
use ffx_component::rcs::{connect_to_lifecycle_controller_f, connect_to_realm_query_f};
use ffx_component_stop_args::ComponentStopCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use target_holders::fdomain::RemoteControlProxyHolder;

#[derive(FfxTool)]
pub struct StopTool {
    #[command]
    cmd: ComponentStopCommand,
    rcs: RemoteControlProxyHolder,
}

fho::embedded_plugin!(StopTool);

#[async_trait(?Send)]
impl FfxMain for StopTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        let lifecycle_controller = connect_to_lifecycle_controller_f(&self.rcs).await?;
        let realm_query = connect_to_realm_query_f(&self.rcs).await?;

        // All errors from component_debug library are user-visible.
        stop_cmd(self.cmd.query, lifecycle_controller, realm_query, writer)
            .await
            .map_err(|e| ffx_error!(e))?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fdomain_client::fidl::ServerEnd;

    use fdomain_fuchsia_sys2 as fsys_f;
    use ffx_writer::TestBuffers;
    use futures::TryStreamExt;

    #[fuchsia::test]
    async fn test_stop() -> anyhow::Result<()> {
        let client = fdomain_local::local_client_empty();
        let moniker = "core/test".to_string();

        let mut capability_handlers = std::collections::HashMap::new();

        // Handler for RealmQuery
        let client_clone = client.clone();
        capability_handlers.insert(
            "svc/fuchsia.sys2.RealmQuery.root".to_string(),
            Box::new(move |server_channel| {
                let client = client_clone.clone();
                let mut rq_stream =
                    ServerEnd::<fsys_f::RealmQueryMarker>::new(server_channel).into_stream();
                fuchsia_async::Task::local(async move {
                    while let Ok(Some(rq_req)) = rq_stream.try_next().await {
                        match rq_req {
                            fsys_f::RealmQueryRequest::GetAllInstances { responder } => {
                                let (client_end, mut iterator_stream) =
                                    client
                                        .create_request_stream::<fsys_f::InstanceIteratorMarker>();
                                fuchsia_async::Task::local(async move {
                                    if let Ok(Some(fsys_f::InstanceIteratorRequest::Next {
                                        responder,
                                    })) = iterator_stream.try_next().await
                                    {
                                        responder
                                            .send(&[fsys_f::Instance {
                                                moniker: Some("core/test".to_string()),
                                                url: Some(
                                                    "fuchsia-pkg://fuchsia.com/test#meta/test.cml"
                                                        .to_string(),
                                                ),
                                                ..Default::default()
                                            }])
                                            .unwrap();
                                    }
                                    if let Ok(Some(fsys_f::InstanceIteratorRequest::Next {
                                        responder,
                                    })) = iterator_stream.try_next().await
                                    {
                                        responder.send(&[]).unwrap();
                                    }
                                })
                                .detach();
                                responder.send(Ok(client_end)).unwrap();
                            }
                            _ => {}
                        }
                    }
                })
                .detach();
            }) as Box<dyn Fn(fdomain_client::Channel) + 'static>,
        );

        // Handler for LifecycleController
        let moniker_clone = moniker.clone();
        capability_handlers.insert(
            "svc/fuchsia.sys2.LifecycleController.root".to_string(),
            Box::new(move |server_channel| {
                let mut lc_stream =
                    ServerEnd::<fsys_f::LifecycleControllerMarker>::new(server_channel)
                        .into_stream();
                let expected_moniker = moniker_clone.clone();
                fuchsia_async::Task::local(async move {
                    while let Ok(Some(lc_req)) = lc_stream.try_next().await {
                        match lc_req {
                            fsys_f::LifecycleControllerRequest::StopInstance {
                                moniker,
                                responder,
                                ..
                            } => {
                                assert_eq!(moniker, expected_moniker);
                                responder.send(Ok(())).unwrap();
                            }
                            _ => {}
                        }
                    }
                })
                .detach();
            }) as Box<dyn Fn(fdomain_client::Channel) + 'static>,
        );

        let config = testing_lib::FakeRcsConfig {
            components: vec![],
            identify_host_handler: None,
            identify_host_response: None,
            capability_handlers,
        };

        let rcs = testing_lib::setup_fake_rcs(client.clone(), config);

        let tool = StopTool { cmd: ComponentStopCommand { query: moniker }, rcs: rcs.into() };

        let buffers = TestBuffers::default();
        let writer = SimpleWriter::new_test(&buffers);

        tool.main(writer).await.expect("tool failed");

        let output = buffers.into_stdout_str();
        assert!(output.contains("Stopped component instance!"));

        Ok(())
    }
}
