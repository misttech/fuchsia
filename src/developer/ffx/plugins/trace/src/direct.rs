// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::progress_reader::ProgressReader;
use crate::{SessionManagerProxyType, TraceData};
use anyhow::Result;
use async_fs::File;
use errors::ffx_bail;
use fdomain_client::fidl::Proxy;
use fdomain_fuchsia_tracing_controller::{RecordingError, TraceConfig, TraceOptions};
use ffx_config::EnvironmentContext;
use fho::{bug, return_bug, return_user_error};
use std::time::Duration;
use trace_task_fdomain::TraceTask;

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
                            .map(|t| trace_task_fdomain::Trigger {
                                action: t
                                    .action
                                    .as_ref()
                                    .map(|_| trace_task_fdomain::TriggerAction::Terminate),
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

pub(crate) async fn trace_on_reboot(
    proxy: SessionManagerProxyType,
    options: TraceOptions,
    trace_config: TraceConfig,
) -> Result<()> {
    if let SessionManagerProxyType::SessionManager(session_manager_proxy) = proxy {
        let r = session_manager_proxy.start_trace_session_on_boot(&trace_config, &options).await?;
        match r {
            Ok(_task_id) => Ok(()),
            Err(e) => Err(anyhow::anyhow!("Error starting trace: {e:?}")),
        }
    } else {
        ffx_bail!("SessionManager proxy is not available on device.")
    }
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
        if let SessionManagerProxyType::SessionManager(session_mgr_proxy) = trace_proxy {
            let (client, server) = session_mgr_proxy.domain().create_stream_socket();
            let join_result = futures::try_join!(download_trace(client, output_file), async {
                log::info!("Calling end_session.");
                // Always pass 0 for the session id, multiple sessions are not supported (yet).
                let r = session_mgr_proxy.end_trace_session(0, server).await.map_err(Into::into);
                log::debug!("Done.");
                r
            });
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

async fn download_trace(
    read_socket: fdomain_client::Socket,
    output_file: &str,
) -> Result<u64, anyhow::Error> {
    let mut output =
        File::create(&output_file).await.map_err(|e| bug!("Could not create output file: {e}"))?;
    use futures::io;
    use std::time::Instant;
    log::info!("Starting local copy to {output_file}.");
    let start_time = Instant::now();
    let mut progress_reader = ProgressReader::new(read_socket);
    let result = io::copy(&mut progress_reader, &mut output).await;

    if let Ok(bytes) = &result {
        let duration = start_time.elapsed();
        let kbytes = *bytes / 1024;

        let rate =
            if duration.as_secs_f64() > 0. { kbytes as f64 / duration.as_secs_f64() } else { 0.0 };
        progress_reader.status_update(
            format!("Total size: {kbytes}kB, Duration: {duration:?}, Rate: {rate:.2} kB/s"),
            true,
        );
    }
    log::info!("Copy done");
    result.map_err(Into::into)
}
