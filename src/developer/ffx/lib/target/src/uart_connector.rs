// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Connector implementation for physical targets connected via UART serial transport.
//!
//! Provides [`UartConnector`], which resolves UART device paths to active driver
//! daemon Unix domain sockets, and establishes an [`FDomainConnection`] over the
//! multiplexed serial link.

use crate::Resolution;
use crate::target_connector::{
    BUFFER_SIZE, FDomainConnection, TargetConnection, TargetConnectionError, TargetConnector,
};
use errors::FfxError;
use ffx_config::{EnvironmentContext, TryFromEnvContext};
use fidl_fuchsia_developer_ffx as ffx;
use futures::future::LocalBoxFuture;
use std::fmt::Debug;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use target_errors::FfxTargetError;
use tokio::io::BufReader;

const CONNECTION_ATTEMPTS: u32 = 20;
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Manages connecting to a Fuchsia target over a UART serial interface.
///
/// Handles target resolution from an endpoint (such as `/dev/ttyUSB0`),
/// locates or validates the corresponding driver daemon Unix domain socket, and
/// implements [`TargetConnector`] to instantiate an FDomain connection.
pub struct UartConnector {
    pub(crate) endpoint: String,
    env_context: EnvironmentContext,
}

impl Debug for UartConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UartConnector").field("endpoint", &self.endpoint).finish()
    }
}

impl UartConnector {
    /// Creates a new `UartConnector` for the specified UART target endpoint.
    ///
    /// Resolves `endpoint` as an explicit device endpoint via `uart_driver_api`,
    /// verifies that the underlying path points to an existing character device
    /// or Unix domain socket, and stores the environment context for driver socket
    /// resolution.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The serial device node path (e.g. `/dev/ttyUSB0`) or socket path.
    /// * `env_context` - The active FFX environment context used to resolve config
    ///   and shared data paths.
    ///
    /// # Errors
    ///
    /// Returns [`FfxTargetError::OpenTargetError`] with `TargetNotFound` if the
    /// target endpoint cannot be parsed, the filesystem path does not exist, or
    /// the target is neither a character device nor a Unix domain socket.
    pub fn new(endpoint: String, env_context: &EnvironmentContext) -> Result<Self, FfxTargetError> {
        let resolved_path = uart_driver_api::parse_target_endpoint(&endpoint, env_context)
            .map_err(|_| target_not_found(&endpoint))?;
        let resolved_endpoint = resolved_path.to_string_lossy().to_string();

        validate_device_path(&resolved_endpoint, &endpoint)?;

        Ok(Self { endpoint: resolved_endpoint, env_context: env_context.clone() })
    }

    fn resolve_socket_path(&self) -> Result<PathBuf, TargetConnectionError> {
        let target_path = PathBuf::from(&self.endpoint);
        let derived_socket =
            uart_driver_api::get_socket_path_from_target_path(&target_path, &self.env_context);
        if is_socket_path(&self.endpoint) {
            if let Ok(ref sock) = derived_socket {
                if sock.to_str().is_some_and(is_socket_path) {
                    return Ok(sock.clone());
                }
            }
            Ok(target_path)
        } else {
            derived_socket.map_err(|e| TargetConnectionError::Fatal(e.into()))
        }
    }
}

fn target_not_found(original_endpoint: &str) -> FfxTargetError {
    FfxTargetError::OpenTargetError {
        err: ffx::OpenTargetError::TargetNotFound,
        target: Some(format!("uart:{}", original_endpoint)),
        targets: vec![],
        target_source: None,
    }
}

fn validate_device_path(path: &str, original_endpoint: &str) -> Result<(), FfxTargetError> {
    let metadata = std::fs::metadata(path).map_err(|_| target_not_found(original_endpoint))?;
    let file_type = metadata.file_type();
    if !file_type.is_char_device() && !file_type.is_socket() {
        return Err(target_not_found(original_endpoint));
    }
    Ok(())
}

/// Errors that can occur when connecting to a UART driver socket.
#[derive(thiserror::Error, Debug)]
pub enum UartConnectionError {
    /// Failed to connect to the Unix domain socket within the retry limit.
    #[error(
        "Failed to connect to UART driver socket at {path} (ensure the UART driver is running via 'ffx uart connect'): {source}"
    )]
    ConnectDriverSocket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

async fn connect_driver_socket(
    socket_path: &Path,
) -> Result<tokio::net::UnixStream, TargetConnectionError> {
    let mut attempts = 0;
    loop {
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                let kind = e.kind();
                let err = UartConnectionError::ConnectDriverSocket {
                    path: socket_path.to_path_buf(),
                    source: e,
                }
                .into();
                if kind == std::io::ErrorKind::PermissionDenied {
                    return Err(TargetConnectionError::Fatal(err));
                }
                attempts += 1;
                if kind == std::io::ErrorKind::NotFound || attempts >= CONNECTION_ATTEMPTS {
                    return Err(TargetConnectionError::NonFatal(err));
                }
                fuchsia_async::Timer::new(CONNECTION_RETRY_INTERVAL).await;
            }
        }
    }
}

impl TargetConnector for UartConnector {
    const CONNECTION_TYPE: &'static str = "UART";

    async fn connect(&mut self) -> Result<TargetConnection, TargetConnectionError> {
        let socket_path = self.resolve_socket_path()?;
        let stream = connect_driver_socket(&socket_path).await?;

        log::info!("Connected to UART driver socket.");
        let (output, input) = stream.into_split();
        let output = BufReader::with_capacity(BUFFER_SIZE, output);
        let (_sender, errors) = async_channel::unbounded();

        Ok(TargetConnection::FDomain(FDomainConnection {
            output: Box::new(output),
            input: Box::new(input),
            errors,
            main_task: None,
        }))
    }
}

impl TryFromEnvContext for UartConnector {
    fn try_from_env_context<'a>(
        env: &'a EnvironmentContext,
    ) -> LocalBoxFuture<'a, ffx_command_error::Result<Self>> {
        Box::pin(async {
            let resolution = Resolution::try_from_env_context(env).await?;
            let endpoint = resolution.uart_endpoint().ok_or_else(|| {
                ffx_command_error::user_error!(
                    "query did not resolve a UART endpoint. Resolved the following: {:?}",
                    resolution,
                )
            })?;
            let conn = UartConnector::new(endpoint, env).map_err(|e| {
                let ffx_err: FfxError = e.into();
                ffx_command_error::Error::from(ffx_err)
            })?;
            Ok(conn)
        })
    }
}

fn is_socket_path(path_str: &str) -> bool {
    std::fs::metadata(path_str).map(|m| m.file_type().is_socket()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_is_socket_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert!(is_socket_path(socket_path.to_str().unwrap()));

        let regular_file = temp_dir.path().join("regular.txt");
        std::fs::write(&regular_file, b"hello").unwrap();
        assert!(!is_socket_path(regular_file.to_str().unwrap()));

        assert!(!is_socket_path("/nonexistent/path.sock"));
    }

    #[fuchsia::test]
    async fn test_connect_driver_socket_fails_fast_on_not_found() {
        let nonexistent = PathBuf::from("/nonexistent/ffx_uart_missing.sock");
        let start = std::time::Instant::now();
        let res = connect_driver_socket(&nonexistent).await;
        assert!(res.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[fuchsia::test]
    async fn test_uart_connector_connect_success() {
        let temp_dir = tempfile::tempdir().unwrap();
        let socket_path = temp_dir.path().join("ffx_uart_test.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        let handle = fuchsia_async::Task::local(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
        });

        let test_env = ffx_config::test_init().unwrap();
        let mut connector = UartConnector {
            endpoint: socket_path.to_string_lossy().to_string(),
            env_context: test_env.context.clone(),
        };

        let conn = connector.connect().await;
        assert!(conn.is_ok());
        handle.await;
    }

    #[fuchsia::test]
    async fn test_uart_connector_resolve_socket_path_derived_vs_direct() {
        let test_env = ffx_config::test_init().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();

        // 1. Direct socket without derived daemon socket returns direct socket path
        let direct_sock = temp_dir.path().join("direct.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&direct_sock).unwrap();
        let connector = UartConnector {
            endpoint: direct_sock.to_string_lossy().to_string(),
            env_context: test_env.context.clone(),
        };
        assert_eq!(connector.resolve_socket_path().unwrap(), direct_sock);

        // 2. When derived socket exists on disk, it is preferred
        let derived_sock =
            uart_driver_api::get_socket_path_from_target_path(&direct_sock, &test_env.context)
                .unwrap();
        std::fs::create_dir_all(derived_sock.parent().unwrap()).unwrap();
        let _derived_listener = std::os::unix::net::UnixListener::bind(&derived_sock).unwrap();
        assert_eq!(connector.resolve_socket_path().unwrap(), derived_sock);
    }
}
