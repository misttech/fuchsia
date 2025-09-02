// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{SessionManagerProxyType, TraceData};
use anyhow::Result;
use async_fs::File;
use ffx_config::EnvironmentContext;
use fho::{bug, return_bug, return_user_error};
use fidl_fuchsia_tracing_controller::{RecordingError, TraceConfig, TraceOptions};
use std::time::Duration;
use trace_task::TraceTask;

pub(crate) async fn trace(
    proxy: SessionManagerProxyType,
    options: TraceOptions,
    trace_config: TraceConfig,
    background: bool,
) -> Result<Option<TraceTask>> {
    let duration = options.duration_ns.map(|d| Duration::from_nanos(d as u64));

    let legacy_task = match proxy {
        SessionManagerProxyType::Provisioner(provisioner_proxy) => {
            let task = TraceTask::new(
                "ffx-trace-direct".into(),
                trace_config.clone(),
                duration,
                options
                    .triggers
                    .map(|tv| {
                        tv.iter()
                            .map(|t| trace_task::Trigger {
                                action: t
                                    .action
                                    .as_ref()
                                    .map(|_| trace_task::TriggerAction::Terminate),
                                alert: t.alert.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or(vec![]),
                options.requested_categories,
                provisioner_proxy,
            )
            .await?;

            Some(task)
        }
        SessionManagerProxyType::SessionManager(session_manager_proxy) => {
            let r = session_manager_proxy.start_trace_session(&trace_config, &options).await?;
            match r {
                Ok(_task_id) => None,
                Err(e) => {
                    return Err(anyhow::anyhow!("Error starting trace: {e:?}"));
                }
            }
        }
    };

    if !background {
        if let Some(trace_duration) = duration {
            fuchsia_async::Timer::new(trace_duration).await;
        }
    }
    Ok(legacy_task)
}

pub(crate) async fn stop_tracing(
    context: &EnvironmentContext,
    trace_task: Option<TraceTask>,
    trace_proxy: SessionManagerProxyType,
    output_file: &str,
) -> fho::Result<TraceData> {
    if let Some(task) = trace_task {
        let output = File::create(output_file).await.map_err(|e| bug!(e))?;
        Ok(TraceData {
            output_file: output_file.to_string(),
            categories: task.config().categories.clone().unwrap_or(vec![]),
            stop_result: task.stop_and_receive_data(output).await.map_err(|e| bug!(e))?,
        })
    } else {
        let mut output = File::create(&output_file)
            .await
            .map_err(|e| bug!("Could not create output file: {e}"))?;
        let (client, server) = fidl::Socket::create_stream();
        let client = fidl::AsyncSocket::from_socket(client);

        if let SessionManagerProxyType::SessionManager(session_mgr_proxy) = trace_proxy {
            let join_result = futures::try_join!(
                async {
                    log::info!("Starting local copy to {output_file}.");
                    let r = futures::io::copy(client, &mut output)
                        .await
                        .map_err(Into::<anyhow::Error>::into);
                    log::info!("Copy done");
                    r
                },
                async {
                    log::info!("Calling end_session.");
                    // Always pass 0 for the session id, multiple sessions are not supported (yet).
                    let r =
                        session_mgr_proxy.end_trace_session(0, server).await.map_err(Into::into);
                    if r.as_ref().ok().map(|r| r.is_ok()).unwrap_or(false) {
                        eprintln!("Writing to {output_file}.");
                    }
                    log::debug!("Done.");
                    r
                }
            );
            match join_result {
                Ok((copy_res, end_res)) => {
                    log::debug!("Copy res is {copy_res:?}");
                    match end_res {
                        Ok((options, stop_result)) => Ok(TraceData {
                            output_file: output_file.to_string(),
                            categories: options.requested_categories.unwrap_or(vec![]),
                            stop_result,
                        }),
                        Err(RecordingError::NoSuchTraceFile) => {
                            return_user_error!("No active traces")
                        }

                        Err(e) => return_bug!(anyhow::anyhow!(
                            "{}",
                            crate::handle_recording_error(context, e, &output_file).await
                        )),
                    }
                }
                Err(e) => return_bug!("join result is {e:?}"),
            }
        } else {
            return_bug!("Unexpected state with no TraceTask, and no SessionManager?");
        }
    }
}
