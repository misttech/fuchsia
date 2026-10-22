// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use ffx_command_error::{Error, Result};
use fho::{FhoEnvironment, TryFromEnv};
use std::time::Duration;
use target_behavior::{
    ConnectionBehavior, DirectConnectError, DirectConnector, FhoTargetEnvironment,
    TargetResolutionError, target_interface,
};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(50);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DiscoveryRetryMode {
    FailOnInitialMiss,
    RetryIndefinitely,
}

/// Returns `true` if `err` wraps an `OpenTargetError::TargetNotFound` target resolution error.
fn is_target_not_found(err: &ffx_command_error::Error) -> bool {
    let ffx_command_error::Error::User(e) = err else {
        return false;
    };
    let Some(errors::FfxError::OpenTargetError { err, .. }) = e.downcast_ref::<errors::FfxError>()
    else {
        return false;
    };
    matches!(
        err.downcast_ref::<target_errors::FfxTargetError>(),
        Some(target_errors::FfxTargetError::OpenTargetError {
            err: fidl_fuchsia_developer_ffx::OpenTargetError::TargetNotFound,
            ..
        })
    )
}

/// A connector lets a tool make multiple attempts to connect to an object. It
/// retains the environment in the tool body to allow this.
#[derive(Clone)]
pub struct Connector<T: TryFromEnv> {
    env: FhoEnvironment,
    target_env: FhoTargetEnvironment,
    _connects_to: std::marker::PhantomData<T>,
}

impl<T> Connector<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    /// Try to get a `T` from the environment. Will wait for the target to
    /// appear if it is non-responsive. If that occurs, `log_target_wait` will
    /// be called prior to waiting.
    pub async fn try_connect(
        &self,
        mut log_target_wait: impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    ) -> Result<T> {
        let behavior =
            self.target_env.init_connection_behavior(self.env.environment_context()).await?;
        // TODO(b/540442481): Clean up naming (e.g. "Direct", "DirectConnector") in a follow-up CL now that direct connection is the only mechanism.
        let ConnectionBehavior::Direct(dc) = &*behavior;
        direct_connector_try_connect::<T>(
            &self.env,
            dc,
            &mut log_target_wait,
            DiscoveryRetryMode::FailOnInitialMiss,
        )
        .await
    }

    /// Try to get a `T` from the environment with an indefinite discovery timeout.
    /// Will wait indefinitely for the target to appear.
    pub async fn try_connect_indefinitely(
        &self,
        mut log_target_wait: impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    ) -> Result<T> {
        let behavior =
            self.target_env.init_connection_behavior_indef(self.env.environment_context()).await?;
        match *behavior {
            ConnectionBehavior::Direct(ref dc) => {
                direct_connector_try_connect::<T>(
                    &self.env,
                    dc,
                    &mut log_target_wait,
                    DiscoveryRetryMode::RetryIndefinitely,
                )
                .await
            }
        }
    }
}

#[async_trait(?Send)]
impl<T> TryFromEnv for Connector<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    type Error = ffx_command_error::Error;
    async fn try_from_env(env: &FhoEnvironment) -> Result<Self, Self::Error> {
        let target_env = target_interface(env);
        Ok(Connector { env: env.clone(), target_env, _connects_to: Default::default() })
    }
}

/// Returns the error to report while waiting to retry if `err` is non-fatal, or `Err` with the
/// fatal error if the command should fail.
fn check_connection_retry(
    target_spec: String,
    err: ffx_command_error::Error,
) -> Result<(Option<String>, ffx_command_error::Error)> {
    let e = err.downcast_non_fatal()?;
    log::debug!("error when trying to connect to target {target_spec}: {e}");
    Ok((Some(target_spec), Error::User(e)))
}

/// Returns the error to report while waiting to retry if resolution failed with `TargetNotFound`
/// and should be retried, or `Err` with the error if the command should fail.
async fn check_resolution_retry(
    err: TargetResolutionError,
    dc: &DirectConnector,
    retry_mode: DiscoveryRetryMode,
) -> Result<(Option<String>, ffx_command_error::Error)> {
    let err = err.into_command_error();
    let should_retry =
        retry_mode == DiscoveryRetryMode::RetryIndefinitely || dc.has_resolved_target().await;
    if should_retry && is_target_not_found(&err) {
        log::debug!("Target not found during resolution, waiting to retry: {err}");
        Ok((None, err))
    } else {
        Err(err)
    }
}

/// Repeatedly resolves and connects to the target until `T::try_from_env` succeeds or a fatal
/// error occurs.
///
/// Each attempt calls [`DirectConnector::connect`], which resolves from `$SHARED_DATA/targets.json`
/// (in strict mode) or the discovery cache / live discovery (in non-strict mode) and re-resolves
/// whenever a previous `Resolution` was invalidated by a failed or timed-out connection attempt.
///
/// Connection and `T::try_from_env` failures are retried only if they are non-fatal. Resolution
/// failures are retried only on `TargetNotFound`, and only when `retry_mode` is
/// `DiscoveryRetryMode::RetryIndefinitely` or when `dc.has_resolved_target()` is `true`
/// (meaning a target was previously resolved and may be temporarily offline across a reboot).
async fn direct_connector_try_connect<T>(
    env: &FhoEnvironment,
    dc: &DirectConnector,
    log_target_wait: &mut impl FnMut(&Option<String>, &Option<Error>) -> Result<()>,
    retry_mode: DiscoveryRetryMode,
) -> Result<T>
where
    T: TryFromEnv<Error = ffx_command_error::Error>,
{
    let mut retry_delay = INITIAL_RETRY_DELAY;
    loop {
        let (target_spec, wait_err) = match dc.connect(env.environment_context()).await {
            Ok((resolution, _conn)) => match T::try_from_env(env).await {
                Ok(val) => return Ok(val),
                Err(err) => check_connection_retry(resolution.target_spec(), err)?,
            },
            Err(DirectConnectError::Connection { target_spec, err }) => {
                check_connection_retry(target_spec, (*err).into_command_error())?
            }
            Err(DirectConnectError::Resolution(err)) => {
                check_resolution_retry(err, dc, retry_mode).await?
            }
        };

        log_target_wait(&target_spec, &Some(wait_err))?;
        fuchsia_async::Timer::new(retry_delay).await;
        retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_config::environment::ExecutableKind;
    use ffx_config::test_env;
    use std::cell::RefCell;
    use std::str::FromStr;
    use target_behavior::ConnectionBehavior;

    #[derive(Debug)]
    struct DummyHolder;

    #[async_trait(?Send)]
    impl TryFromEnv for DummyHolder {
        type Error = ffx_command_error::Error;
        async fn try_from_env(_env: &FhoEnvironment) -> Result<Self, Self::Error> {
            Ok(DummyHolder)
        }
    }

    #[derive(Debug)]
    struct PlaceholderConnector;
    impl ffx_target::TargetConnector for PlaceholderConnector {
        const CONNECTION_TYPE: &'static str = "placeholder";
        async fn connect(
            &mut self,
        ) -> std::result::Result<ffx_target::TargetConnection, ffx_target::TargetConnectionError>
        {
            Ok(ffx_target::TargetConnection::FDomain(ffx_target::FDomainConnection::invalid()))
        }
    }

    // Tests that `Connector<T>` can successfully connect via `try_connect` (using default
    // discovery timeout) and `try_connect_indefinitely` (using indefinite discovery timeout)
    // without errors when a valid direct connection behavior and target environment are configured.
    #[fuchsia::test]
    async fn test_connector_try_connect_and_try_connect_indefinitely() {
        let env = test_env().build().unwrap();
        let fho_env = FhoEnvironment::new_with_args(&env.context, &["some", "test"]);
        let target_env = target_behavior::target_interface(&fho_env);

        let resolution = ffx_target::Resolution::mock(|| unreachable!());
        let conn = ffx_target::Connection::new(PlaceholderConnector).await.unwrap();
        resolution.set_connection_for_test(Some(conn)).await;
        let behavior = ConnectionBehavior::fake_direct_connector(resolution);
        target_env.set_behavior_for_test(behavior);

        let connector =
            Connector::<DummyHolder>::try_from_env(&fho_env).await.expect("create connector");

        let res = connector.try_connect(|_, _| Ok(())).await;
        assert!(res.is_ok());

        let res_indef = connector.try_connect_indefinitely(|_, _| Ok(())).await;
        assert!(res_indef.is_ok());
    }

    #[fuchsia::test]
    async fn test_connector_try_connect_target_not_found_fails_immediately() {
        let env_failure = test_env()
            .user_config("connectivity.enable_network", serde_json::json!(false))
            .user_config("connectivity.enable_usb", serde_json::json!(false))
            .user_config("discovery.mdns.enabled", serde_json::json!(false))
            .build()
            .unwrap();

        let fho_env_failure =
            FhoEnvironment::new_with_args(&env_failure.context, &["some", "test"]);
        let connector_failure =
            Connector::<DummyHolder>::try_from_env(&fho_env_failure).await.unwrap();

        let log_target_wait_called = RefCell::new(false);
        let res_failure = connector_failure
            .try_connect(|_, _| {
                *log_target_wait_called.borrow_mut() = true;
                Ok(())
            })
            .await;

        let err = res_failure.unwrap_err();
        match err {
            ffx_command_error::Error::User(e) => {
                let ffx_err = e.downcast_ref::<errors::FfxError>().expect("Expected FfxError");
                let inner = match ffx_err {
                    errors::FfxError::OpenTargetError { err, .. } => err
                        .downcast_ref::<target_errors::FfxTargetError>()
                        .expect("Expected FfxTargetError"),
                    _ => panic!("Expected FfxError::OpenTargetError"),
                };
                assert!(matches!(
                    inner,
                    target_errors::FfxTargetError::OpenTargetError {
                        err: fidl_fuchsia_developer_ffx::OpenTargetError::TargetNotFound,
                        ..
                    }
                ));
            }
            _ => panic!("Expected Error::User(FfxTargetError)"),
        }
        assert!(!*log_target_wait_called.borrow());
    }

    // Tests that a fatal error returned during `Resolution::get_connection()` (simulated via
    // `Resolution::mock`) fails immediately without retrying.
    #[fuchsia::test]
    async fn test_connector_try_connect_mock_connection_fatal_error_fails_immediately() {
        let env = test_env().build().unwrap();
        let fho_env = FhoEnvironment::new_with_args(&env.context, &["some", "test"]);
        let target_env = target_behavior::target_interface(&fho_env);

        let resolution = ffx_target::Resolution::mock(|| {
            Err(target_errors::FfxTargetError::OpenTargetError {
                err: fidl_fuchsia_developer_ffx::OpenTargetError::TargetNotFound,
                target: None,
                targets: vec![],
                target_source: None,
            }
            .into())
        });
        let behavior = ConnectionBehavior::fake_direct_connector(resolution);
        target_env.set_behavior_for_test(behavior);

        let connector = Connector::<DummyHolder>::try_from_env(&fho_env).await.unwrap();
        let log_target_wait_called = RefCell::new(false);
        let res = connector
            .try_connect(|_, _| {
                *log_target_wait_called.borrow_mut() = true;
                Ok(())
            })
            .await;

        assert!(is_target_not_found(&res.unwrap_err()));
        assert!(!*log_target_wait_called.borrow());
    }

    // Tests that a fatal `TargetNotFound` returned during `Resolution::get_connection()` (such as
    // a missing UART device path) fails immediately even under `try_connect_indefinitely`, which
    // only retries `TargetNotFound` from resolution.
    #[fuchsia::test]
    async fn test_connector_try_connect_indefinitely_connection_target_not_found_fails_immediately()
    {
        let env = test_env().build().unwrap();
        let fho_env = FhoEnvironment::new_with_args(&env.context, &["some", "test"]);
        let target_env = target_behavior::target_interface(&fho_env);

        let resolution = ffx_target::Resolution::mock(|| {
            Err(target_errors::FfxTargetError::OpenTargetError {
                err: fidl_fuchsia_developer_ffx::OpenTargetError::TargetNotFound,
                target: None,
                targets: vec![],
                target_source: None,
            }
            .into())
        });
        let behavior = ConnectionBehavior::fake_direct_connector(resolution);
        target_env.set_behavior_for_test(behavior);

        let connector = Connector::<DummyHolder>::try_from_env(&fho_env).await.unwrap();
        let log_target_wait_called = RefCell::new(false);
        let res = connector
            .try_connect_indefinitely(|_, _| {
                *log_target_wait_called.borrow_mut() = true;
                Ok(())
            })
            .await;

        assert!(is_target_not_found(&res.unwrap_err()));
        assert!(!*log_target_wait_called.borrow());
    }

    // Tests that even when `try_connect_indefinitely` is used (`should_retry_not_found == true`),
    // a non-`TargetNotFound` resolution error (such as missing `-t` in strict mode) fails
    // immediately without retrying.
    #[fuchsia::test]
    async fn test_connector_try_connect_indefinitely_non_target_not_found_resolution_error_fails_immediately()
     {
        let strict_ctx =
            ffx_config::EnvironmentContext::strict(ExecutableKind::Test, Default::default())
                .unwrap();
        let fho_env = FhoEnvironment::new_with_args(&strict_ctx, &["some", "test"]);
        let connector = Connector::<DummyHolder>::try_from_env(&fho_env).await.unwrap();

        let log_target_wait_called = RefCell::new(false);
        let res = connector
            .try_connect_indefinitely(|_, _| {
                *log_target_wait_called.borrow_mut() = true;
                Ok(())
            })
            .await;

        let err = res.unwrap_err();
        assert!(!is_target_not_found(&err));
        assert!(!*log_target_wait_called.borrow());
    }

    #[fuchsia::test]
    async fn test_connector_try_connect_explicit_ip_succeeds() {
        let mut env_success = test_env().build().unwrap();
        env_success.context.override_target_specifier(&Some("127.0.0.1:8022".to_string()));
        let fho_env_success =
            FhoEnvironment::new_with_args(&env_success.context, &["some", "test"]);

        let target_env = target_interface(&fho_env_success);
        let behavior = target_env.init_connection_behavior(&env_success.context).await.unwrap();
        let dc = match &*behavior {
            ConnectionBehavior::Direct(dc) => dc,
        };
        let resolution = dc.resolution().await.unwrap();

        let conn = ffx_target::Connection::new(PlaceholderConnector).await.unwrap();
        resolution.set_connection_for_test(Some(conn)).await;

        let connector_success =
            Connector::<DummyHolder>::try_from_env(&fho_env_success).await.unwrap();
        let log_target_wait_called_success = RefCell::new(false);
        let res_success = connector_success
            .try_connect(|_, _| {
                *log_target_wait_called_success.borrow_mut() = true;
                Ok(())
            })
            .await;

        assert!(res_success.is_ok());
        assert!(!*log_target_wait_called_success.borrow());
    }

    #[fuchsia::test]
    async fn test_connector_retries_non_fatal_and_re_resolves_from_strict_targets_json() {
        let mut builder = test_env();
        let temp_dir = builder.isolate_root();
        let fake_key = temp_dir.join("fake_ssh_key");
        std::fs::write(&fake_key, "fake").unwrap();

        let runtime_args = serde_json::json!({
            "shared_data": temp_dir.to_str().unwrap(),
            "target": { "default": "id:04012C12D45E" },
            "ssh": {
                "path": "/bin/false",
                "priv": fake_key.to_str().unwrap()
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let strict_ctx =
            ffx_config::EnvironmentContext::strict(ExecutableKind::Test, runtime_args).unwrap();

        // Register v1 address (127.0.0.1:8082) in $SHARED_DATA/targets.json.
        let mut target_v1 = ffx_target::TargetInfo::default();
        target_v1.nodename = Some("fuchsia-node".to_string());
        target_v1.serial_number = Some("04012C12D45E".to_string());
        target_v1.target_state = ffx_target::info::TargetState::Product;
        target_v1.addresses = vec![addr::TargetAddr::from_str("127.0.0.1:8082").unwrap()];
        ffx_target::register_targets(&strict_ctx, &[target_v1]).unwrap();

        let fho_env = FhoEnvironment::new_with_args(&strict_ctx, &["some", "test"]);
        let connector = Connector::<DummyHolder>::try_from_env(&fho_env).await.unwrap();
        let wait_specs = RefCell::new(Vec::<Option<String>>::new());

        let res = connector
            .try_connect(|target_spec, _err| {
                let mut specs = wait_specs.borrow_mut();
                specs.push(target_spec.clone());
                if specs.len() == 1 {
                    // Simulate the external orchestrator atomically updating $SHARED_DATA/targets.json
                    // to the new address (127.0.0.1:8083) while ffx waits to retry.
                    let mut target_v2 = ffx_target::TargetInfo::default();
                    target_v2.nodename = Some("fuchsia-node".to_string());
                    target_v2.serial_number = Some("04012C12D45E".to_string());
                    target_v2.target_state = ffx_target::info::TargetState::Product;
                    target_v2.addresses =
                        vec![addr::TargetAddr::from_str("127.0.0.1:8083").unwrap()];
                    ffx_target::register_targets(&strict_ctx, &[target_v2]).unwrap();
                    Ok(())
                } else {
                    Err(ffx_command_error::user_error!("stop after observing updated target"))
                }
            })
            .await;

        assert!(res.is_err());
        assert_eq!(
            *wait_specs.borrow(),
            vec![Some("127.0.0.1:8082".to_string()), Some("127.0.0.1:8083".to_string())]
        );
    }
}
