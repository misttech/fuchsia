// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, Result, anyhow};
use fidl::endpoints::{ProtocolMarker, create_endpoints};
use fidl_fuchsia_power_system as fps;
use fuchsia_async as fasync;
use fuchsia_component::client;
use futures::channel::mpsc;
use futures::{Future, SinkExt, StreamExt, TryStreamExt, future};
use log::{debug, error};

const SUSPEND_BLOCKER_NAME: &str = "timekeeper";

const POWER_ON: u8 = 0xff;
const POWER_OFF: u8 = 0x00;

enum SuspendBlockerResponder {
    BeforeSuspend(fps::SuspendBlockerBeforeSuspendResponder),
    AfterResume(fps::SuspendBlockerAfterResumeResponder),
}

pub async fn manage(activity_signal: mpsc::Sender<super::Command>) -> Result<fasync::Task<()>> {
    let governor_proxy = client::connect_to_protocol::<fps::ActivityGovernorMarker>()
        .with_context(|| {
            format!("while connecting to: {:?}", fps::ActivityGovernorMarker::DEBUG_NAME)
        })?;

    manage_internal(governor_proxy, activity_signal, management_loop).await
}

async fn manage_internal<F, G>(
    governor_proxy: fps::ActivityGovernorProxy,
    activity: mpsc::Sender<super::Command>,
    // Injected in tests.
    loop_fn: F,
) -> Result<fasync::Task<()>>
where
    G: Future<Output = fasync::Task<()>>,
    F: Fn(fps::SuspendBlockerRequestStream, mpsc::Sender<super::Command>) -> G,
{
    let (suspend_blocker_client, suspend_blocker_server) =
        create_endpoints::<fps::SuspendBlockerMarker>();
    let result = governor_proxy
        .register_suspend_blocker(fps::ActivityGovernorRegisterSuspendBlockerRequest {
            suspend_blocker: Some(suspend_blocker_client),
            name: Some(SUSPEND_BLOCKER_NAME.into()),
            ..Default::default()
        })
        .await
        .context("while calling fuchsia.power.system.ActivityGovernor/RegisterSuspendBlocker")?;
    let suspend_blocker = suspend_blocker_server.into_stream();
    match result {
        Ok(_) => Ok(loop_fn(suspend_blocker, activity).await),
        Err(e) => Err(anyhow!("error while registering suspend blocker: {:?}", e)),
    }
}

// Loop around and react to SuspendBlocker messages. Use separate tasks to ensure
// we can insert a power transition process in between.
//
// Returns the task spawned for transition control.
async fn management_loop(
    mut suspend_blocker_stream: fps::SuspendBlockerRequestStream,
    mut activity: mpsc::Sender<super::Command>,
) -> fasync::Task<()> {
    // The Sender is used to ensure that rcv_task does not send before send_task
    // is done.
    let (mut send, mut rcv) = mpsc::channel::<(u8, mpsc::Sender<()>)>(1);

    let rcv_task = fasync::Task::local(async move {
        loop {
            let Ok(Some(request)) = suspend_blocker_stream.try_next().await else {
                error!("error while waiting for suspend blocker request, bailing");
                break;
            };
            let result = handle_suspend_blocker_request(request).await;
            match result {
                Ok((level, responder)) => {
                    let (s, mut r) = mpsc::channel::<()>(1);
                    if let Err(e) = send.send((level, s)).await {
                        error!("error while processing power level, bailing: {:?}", e);
                        break;
                    }
                    // Wait until send_task propagates the new required level.
                    r.next().await.unwrap();
                    // Respond to the SuspendBlockerRequest, indicating the level transition is
                    // complete.
                    match responder {
                        SuspendBlockerResponder::BeforeSuspend(responder) => {
                            responder.send().expect("BeforeSuspend resp failed");
                        }
                        SuspendBlockerResponder::AfterResume(responder) => {
                            responder.send().expect("AfterResume resp failed");
                        }
                    }
                }
                Err(e) => {
                    error!("error while watching level, bailing: {:?}", e);
                    break;
                }
            }
        }
        debug!("no longer watching required level");
    });
    let send_task = fasync::Task::local(async move {
        while let Some((new_level, mut s)) = rcv.next().await {
            let suspended = match new_level {
                POWER_OFF => {
                    debug!("new required level: power off");
                    Some(true)
                }
                POWER_ON => {
                    debug!("new required level: power on");
                    Some(false)
                }
                _ => {
                    error!("invalid required level: {}", new_level);
                    None
                }
            };
            if let Some(suspended) = suspended {
                let (done_tx, mut done_rx) = mpsc::channel(1);
                if let Err(e) = activity
                    .send(super::Command::PowerManagement { suspended, done: done_tx })
                    .await
                {
                    error!("error sending PowerManagement command: {:?}", e);
                } else if done_rx.next().await.is_none() {
                    error!("PowerManagement command responder dropped without signaling");
                }
            }
            // Signal rcv_task that level transition handling is complete.
            s.send(()).await.unwrap();
        }
        debug!("no longer reporting required level");
    });
    fasync::Task::local(async move {
        future::join(rcv_task, send_task).await;
    })
}

async fn handle_suspend_blocker_request(
    request: fps::SuspendBlockerRequest,
) -> Result<(u8, SuspendBlockerResponder), Error> {
    match request {
        fps::SuspendBlockerRequest::BeforeSuspend { responder } => {
            return Ok((POWER_OFF, SuspendBlockerResponder::BeforeSuspend(responder)));
        }
        fps::SuspendBlockerRequest::AfterResume { responder } => {
            return Ok((POWER_ON, SuspendBlockerResponder::AfterResume(responder)));
        }
        fps::SuspendBlockerRequest::_UnknownMethod { .. } => {
            return Err(Error::msg("SuspendBlockerRequest::_UnknownMethod received"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command;
    use fidl::endpoints;
    use log::debug;

    #[fuchsia::test]
    async fn propagate_level() {
        let (suspend_blocker_client, suspend_blocker_server) =
            create_endpoints::<fps::SuspendBlockerMarker>();
        let suspend_blocker = suspend_blocker_server.into_stream();
        let suspend_blocker_proxy = suspend_blocker_client.into_proxy();
        let (activity_s, mut activity_r) = mpsc::channel::<Command>(1);

        // Management loop is also asynchronous.
        fasync::Task::local(async move {
            management_loop(suspend_blocker, activity_s).await.await;
        })
        .detach();

        let (mut observed_tx, mut observed_rx) = mpsc::channel::<bool>(3);
        fasync::Task::local(async move {
            while let Some(cmd) = activity_r.next().await {
                match cmd {
                    Command::PowerManagement { suspended, mut done } => {
                        observed_tx.send(suspended).await.unwrap();
                        done.send(()).await.unwrap();
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            }
        })
        .detach();

        assert!(suspend_blocker_proxy.after_resume().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(false));

        assert!(suspend_blocker_proxy.before_suspend().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(true));

        assert!(suspend_blocker_proxy.after_resume().await.is_ok());
        assert_eq!(observed_rx.next().await, Some(false));
    }

    async fn empty_loop(
        _: fps::SuspendBlockerRequestStream,
        mut activity: mpsc::Sender<Command>,
    ) -> fasync::Task<()> {
        let (done_tx, _done_rx) = mpsc::channel(1);
        let _ = activity.send(Command::PowerManagement { suspended: false, done: done_tx }).await;
        fasync::Task::local(async move {})
    }

    #[fuchsia::test]
    async fn test_manage_internal() {
        let (g_proxy, mut g_stream) =
            endpoints::create_proxy_and_stream::<fps::ActivityGovernorMarker>();
        let (_activity_s, mut activity_r) = mpsc::channel::<Command>(1);

        // Run the server side activity governor.
        fasync::Task::local(async move {
            while let Some(request) = g_stream.next().await {
                match request {
                    Ok(fps::ActivityGovernorRequest::RegisterSuspendBlocker {
                        payload: _,
                        responder,
                    }) => {
                        let (_unused_server_token, client_token) = fps::LeaseToken::create();
                        responder.send(Ok(client_token)).expect("never fails");
                    }
                    Ok(_) | Err(_) => unimplemented!(),
                }
            }
            debug!("governor server side test exiting");
        })
        .detach();

        fasync::Task::local(async move {
            manage_internal(g_proxy, _activity_s, empty_loop).await.unwrap().await;
        })
        .detach();

        activity_r.next().await.unwrap();
    }
}
