// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_cpu_profiler::{SessionManagerRequest, SessionManagerRequestStream};
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::StreamExt;
use log::{error, info};
use std::sync::Arc;
enum ManagedSession {
    Background(session::BackgroundSession),
    Attached(fidl_fuchsia_cpu_profiler::SessionProxy),
    Starting,
}

mod session;

enum IncomingRequest {
    SessionManager(SessionManagerRequestStream),
}

struct SessionManager {
    next_task_id: u64,
    current_session: Option<ManagedSession>,
}

impl SessionManager {
    fn new() -> Self {
        Self { next_task_id: 1, current_session: None }
    }
}

async fn handle_session_manager_request_stream(
    mut stream: SessionManagerRequestStream,
    manager: Arc<Mutex<SessionManager>>,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(SessionManagerRequest::StartSession { payload, responder }) => {
                let task_id = {
                    let mut mgr = manager.lock();
                    if mgr.current_session.is_some() {
                        let response =
                            Err(fidl_fuchsia_cpu_profiler::ManagerError::TooManySessions);
                        error!("There is already an existing current session.");
                        if let Err(e) = responder.send(response) {
                            error!("Failed to send StartSession response: {:?}", e);
                        }
                        continue;
                    }
                    let id = mgr.next_task_id;
                    mgr.next_task_id += 1;
                    mgr.current_session = Some(ManagedSession::Starting);
                    id
                };
                info!("StartSession called. Assigned task_id {}.", task_id);

                let mgr_clone = manager.clone();
                let config = if let Some(config) = payload.config {
                    config
                } else {
                    error!("No config available when calling StartSession.");
                    let response =
                        Err(fidl_fuchsia_cpu_profiler::ManagerError::InvalidConfiguration);
                    if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                        error!("Failed to send StartSession error response: {:?}", e);
                    }
                    continue;
                };

                match session::BackgroundSession::start(task_id, config).await {
                    Ok(bg_session) => {
                        let mut mgr = mgr_clone.lock();
                        mgr.current_session = Some(ManagedSession::Background(bg_session));

                        let response =
                            Ok(fidl_fuchsia_cpu_profiler::SessionManagerStartSessionResponse {
                                task_id: Some(task_id),
                                ..Default::default()
                            });

                        if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                            error!("Failed to send StartSession response: {e:?}");
                        }
                    }
                    Err(e) => {
                        let mut mgr = mgr_clone.lock();
                        mgr.current_session = None;
                        error!("Failed to start background session {}: {:?}", task_id, e);
                        let response = Err(fidl_fuchsia_cpu_profiler::ManagerError::Start);
                        if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                            error!("Failed to send StartSession error response: {:?}", e);
                        }
                    }
                }
            }
            Ok(SessionManagerRequest::StopSession { payload, responder }) => {
                info!("StopSession called.");

                let active = {
                    let mut mgr = manager.lock();
                    mgr.current_session.take()
                };

                match active {
                    Some(ManagedSession::Background(mut s)) => {
                        let output = match payload.output {
                            Some(o) => o,
                            None => {
                                error!("StopSession called without an output socket");
                                let response = Err(
                                    fidl_fuchsia_cpu_profiler::ManagerError::InvalidConfiguration,
                                );
                                if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                                    error!("Failed to send StopSession response margin: {:?}", e);
                                }
                                continue;
                            }
                        };

                        let response = match s.stop_and_stream(output).await {
                            Ok(res) => Ok(res),
                            Err(e) => {
                                error!("Failed to stop and stream session {}: {:?}", s.task_id, e);
                                Err(fidl_fuchsia_cpu_profiler::ManagerError::Stop)
                            }
                        };

                        if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                            error!("Failed to send StopSession response: {e:?}");
                        }
                    }
                    _ => {
                        let response = Err(fidl_fuchsia_cpu_profiler::ManagerError::NoSuchTask);
                        error!("No active session to stop.");
                        if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                            error!("Failed to send StopSession response: {:?}", e);
                        }
                    }
                }
            }
            Ok(SessionManagerRequest::AbortSession { responder, .. }) => {
                info!("AbortSession called.");

                let active = {
                    let mut mgr = manager.lock();
                    mgr.current_session.take()
                };

                let response = match active {
                    Some(ManagedSession::Background(mut s)) => {
                        if let Err(e) = s.abort().await {
                            error!("Failed to abort session {}: {:?}", s.task_id, e);
                        }
                        Ok(())
                    }
                    Some(ManagedSession::Attached(proxy)) => {
                        // Technically, this should be invalid since abort is only for background
                        // sessions. However, since this is a cleanup command, let's try to do the
                        // right thing and clean up the session.
                        if let Err(e) = proxy.reset().await {
                            error!("Failed to abort attached session: {:?}", e);
                        }
                        Ok(())
                    }
                    Some(ManagedSession::Starting) | None => {
                        error!("No active session to abort.");
                        Err(fidl_fuchsia_cpu_profiler::ManagerError::NoSuchTask)
                    }
                };

                if let Err(e) = responder.send(response) {
                    error!("Failed to send AbortSession response: {:?}", e);
                }
            }
            Ok(SessionManagerRequest::Status { responder }) => {
                info!("Status called.");
                let response = Ok(fidl_fuchsia_cpu_profiler::SessionManagerStatusResponse {
                    sessions: Some(
                        manager
                            .lock()
                            .current_session
                            .as_ref()
                            .into_iter()
                            .filter_map(|active| match active {
                                ManagedSession::Background(session) => {
                                    Some(fidl_fuchsia_cpu_profiler::ProfilerStatus {
                                        task_id: Some(session.task_id),
                                        ..Default::default()
                                    })
                                }
                                ManagedSession::Attached(_) => {
                                    Some(fidl_fuchsia_cpu_profiler::ProfilerStatus::default())
                                }
                                ManagedSession::Starting => None,
                            })
                            .collect(),
                    ),
                    ..Default::default()
                });
                if let Err(e) = responder.send(response.as_ref().map_err(|e| *e)) {
                    error!("Failed to send Status response: {:?}", e);
                }
            }

            Ok(SessionManagerRequest::Configure { payload, responder }) => {
                info!("Configure called directly.");

                let proxy = {
                    let mut mgr = manager.lock();
                    if mgr.current_session.is_some() {
                        let _ = responder
                            .send(Err(fidl_fuchsia_cpu_profiler::SessionConfigureError::BadState));
                        continue;
                    }

                    let proxy = match fuchsia_component::client::connect_to_protocol::<
                        fidl_fuchsia_cpu_profiler::SessionMarker,
                    >() {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to connect to underlying Session: {:?}", e);
                            if let Err(e) = responder.send(Err(
                                fidl_fuchsia_cpu_profiler::SessionConfigureError::BadState,
                            )) {
                                error!("Failed to send Configure response: {:?}", e);
                            }
                            continue;
                        }
                    };

                    mgr.current_session = Some(ManagedSession::Attached(proxy.clone()));
                    proxy
                };

                match proxy.configure(payload).await {
                    Ok(res) => {
                        let mut mgr = manager.lock();
                        if res.is_err() {
                            mgr.current_session = None;
                        }
                        if let Err(e) = responder.send(res) {
                            error!("Failed to send Configure response: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Configure FIDL error: {:?}", e);
                        manager.lock().current_session = None;
                        if let Err(e) = responder
                            .send(Err(fidl_fuchsia_cpu_profiler::SessionConfigureError::BadState))
                        {
                            error!("Failed to send Configure response: {:?}", e);
                        }
                    }
                }
            }
            Ok(SessionManagerRequest::Start { payload, responder }) => {
                info!("Start called.");
                let proxy = {
                    let mgr = manager.lock();
                    match &mgr.current_session {
                        Some(ManagedSession::Attached(p)) => p.clone(),
                        Some(ManagedSession::Background(_)) => {
                            error!("Start called on a background session.");
                            if let Err(e) = responder
                                .send(Err(fidl_fuchsia_cpu_profiler::SessionStartError::BadState))
                            {
                                error!("Failed to send Start response: {:?}", e);
                            }
                            continue;
                        }
                        _ => {
                            error!("No active  session to start.");
                            if let Err(e) = responder
                                .send(Err(fidl_fuchsia_cpu_profiler::SessionStartError::BadState))
                            {
                                error!("Failed to send Start response: {:?}", e);
                            }
                            continue;
                        }
                    }
                };

                let res = proxy
                    .start(&payload)
                    .await
                    .unwrap_or(Err(fidl_fuchsia_cpu_profiler::SessionStartError::BadState));
                if let Err(e) = responder.send(res) {
                    error!("Failed to send Start response: {:?}", e);
                }
            }
            Ok(SessionManagerRequest::Stop { responder }) => {
                info!("Stop called.");
                let proxy = {
                    let mgr = manager.lock();
                    match &mgr.current_session {
                        Some(ManagedSession::Attached(p)) => p.clone(),
                        Some(ManagedSession::Background(_)) => {
                            error!(
                                "Stop called on a background session. Background sessions require StopSession."
                            );
                            if let Err(e) = responder.send(&Default::default()) {
                                error!("Failed to send Start response: {:?}", e);
                            }
                            continue;
                        }
                        _ => {
                            error!("No active  session to stop.");
                            if let Err(e) = responder.send(&Default::default()) {
                                error!("Failed to send Start response: {:?}", e);
                            }
                            continue;
                        }
                    }
                };

                match proxy.stop().await {
                    Ok(res) => {
                        if let Err(e) = responder.send(&res) {
                            error!("Failed to send Stop response: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Stop FIDL error: {:?}", e);
                        // There is no error on stop, so respond with an emptu result.
                        if let Err(e) = responder.send(&Default::default()) {
                            error!("Failed to send Stop response: {:?}", e);
                        }
                    }
                }
            }
            Ok(SessionManagerRequest::Reset { responder }) => {
                info!("Reset called.");
                let proxy = {
                    let mut mgr = manager.lock();
                    match &mgr.current_session {
                        // Only take the current session if it is attached.
                        Some(ManagedSession::Attached(_)) => {
                            if let Some(ManagedSession::Attached(p)) = mgr.current_session.take() {
                                Some(p)
                            } else {
                                error!("No active session proxy to reset.");
                                None
                            }
                        }
                        Some(ManagedSession::Background(_)) => {
                            error!(
                                "Stop called on a background session. Background sessions require StopSession."
                            );
                            None
                        }
                        _ => {
                            error!("No active  session to stop.");
                            None
                        }
                    }
                };
                if let Some(proxy) = proxy {
                    // reset does not return an error, so ignore it.
                    let _ = proxy.reset().await;
                }
                if let Err(e) = responder.send() {
                    error!("Failed to send Reset response: {:?}", e);
                }
            }
            Err(e) => {
                error!("SessionManagerRequestStream error: {:?}", e);
            }
        }
    }
}

#[fuchsia::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    info!("profiler_session_manager started");

    let manager = Arc::new(Mutex::new(SessionManager::new()));

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(IncomingRequest::SessionManager);
    fs.take_and_serve_directory_handle()?;

    fs.for_each_concurrent(None, |request| async {
        let manager_clone = manager.clone();
        match request {
            IncomingRequest::SessionManager(stream) => {
                handle_session_manager_request_stream(stream, manager_clone).await;
            }
        }
    })
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl;
    use fidl_fuchsia_cpu_profiler::{ManagerError, SessionManagerMarker};
    use fuchsia_async as fasync;

    #[fuchsia::test]
    async fn test_start_session_no_config() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SessionManagerMarker>();

        let _task = fasync::Task::spawn(handle_session_manager_request_stream(stream, manager));

        let req = fidl_fuchsia_cpu_profiler::SessionManagerStartSessionRequest {
            config: None,
            ..Default::default()
        };
        let result = proxy.start_session(req).await.unwrap();

        assert_eq!(result, Err(ManagerError::InvalidConfiguration));
    }

    #[fuchsia::test]
    async fn test_start_session_already_running() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        // Pre-populate with a placeholder session
        {
            let mut mgr = manager.lock();
            let (proxy, _) =
                fidl::endpoints::create_proxy::<fidl_fuchsia_cpu_profiler::SessionMarker>();
            mgr.current_session = Some(ManagedSession::Background(
                session::BackgroundSession::new_for_test(1, proxy),
            ));
        }

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SessionManagerMarker>();
        let _task = fasync::Task::spawn(handle_session_manager_request_stream(stream, manager));

        let req = fidl_fuchsia_cpu_profiler::SessionManagerStartSessionRequest {
            config: Some(fidl_fuchsia_cpu_profiler::Config::default()),
            ..Default::default()
        };
        let result = proxy.start_session(req).await.unwrap();

        assert_eq!(result, Err(ManagerError::TooManySessions));
    }

    #[fuchsia::test]
    async fn test_stop_session_not_found() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SessionManagerMarker>();
        let _task = fasync::Task::spawn(handle_session_manager_request_stream(stream, manager));

        let (s, _) = zx::Socket::create_stream();
        let req = fidl_fuchsia_cpu_profiler::SessionManagerStopSessionRequest {
            task_id: Some(99),
            output: Some(s),
            ..Default::default()
        };
        let result = proxy.stop_session(req).await.unwrap();

        assert_eq!(result, Err(ManagerError::NoSuchTask));
    }

    #[fuchsia::test]
    async fn test_abort_session_not_found() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SessionManagerMarker>();
        let _task = fasync::Task::spawn(handle_session_manager_request_stream(stream, manager));

        let req = fidl_fuchsia_cpu_profiler::SessionManagerAbortSessionRequest {
            task_id: Some(99),
            ..Default::default()
        };
        let result = proxy.abort_session(&req).await.unwrap();

        assert_eq!(result, Err(ManagerError::NoSuchTask));
    }

    #[fuchsia::test]
    async fn test_status_empty() {
        let manager = Arc::new(Mutex::new(SessionManager::new()));
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<SessionManagerMarker>();
        let _task = fasync::Task::spawn(handle_session_manager_request_stream(stream, manager));

        let result = proxy.status().await.unwrap();
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.sessions.unwrap().len(), 0);
    }
}
