// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::injection::Injection;
use ffx_command_error::Result;
use ffx_config::{EnvironmentContext, TryFromEnvContext};
use ffx_core::Injector;
use ffx_target::Resolution;
use fho::TryFromEnv;
use std::fmt;
use std::sync::{Arc, Mutex};

mod injection;

#[derive(Clone)]
pub enum ConnectionBehavior {
    DaemonConnector(Arc<dyn Injector>),
    DirectConnector(Arc<Resolution>),
}

// Manually implement Debug here so we can skip implementing
// Debug on the traits of the variant data.
impl fmt::Debug for ConnectionBehavior {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::DaemonConnector(_) => "DaemonConnector",
            Self::DirectConnector(_) => "DirectConnector",
        };
        write!(f, "{name}")
    }
}

#[derive(Clone, Default)]
pub struct FhoTargetEnvironment {
    // Wrap real FhoTargetEnvironment in Arc because fho_env::get_interface() requires
    // our interface to be cloneable.
    inner: Arc<FhoTargetEnvironmentInner>,
}

impl FhoTargetEnvironment {
    /// This attempts to wrap errors around a potential failure in the underlying connection being
    /// used to facilitate FIDL protocols. This should NOT be used by developers, this is intended
    /// to be used outside of the scope of an ffx subtool (outside of the `main` function).
    fn maybe_wrap_connection_errors(&self, err: fho::Error) -> fho::Error {
        self.inner.maybe_wrap_connection_errors(err)
    }

    /// Initialize either a daemon connection or a direct connection,
    /// depending on how the tool was run. If will be a direct connection
    /// if any of:
    ///   * we are in strict mode
    ///   * the `connectivity.direct=true` config is set (e.g. with "ffx -d")
    pub async fn init_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        self.inner.init_connection_behavior(context).await
    }

    /// Explicitly create direct connection behavior.
    pub async fn init_direct_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        self.inner.init_direct_connection_behavior(context).await
    }

    /// Explicitly create daemon connection behavior, for subtools such as `ffx daemon echo`
    /// which we guarantee will use the daemon, irrespective of the configured connection type.
    /// Returns an error when in strict mode.
    pub async fn init_daemon_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        self.inner.init_daemon_connection_behavior(context).await
    }

    pub fn set_behavior_for_test(&self, new_behavior: ConnectionBehavior) {
        let _ = self.inner.set_behavior(new_behavior).expect("set_behavior_for_test failed??");
    }

    pub fn behavior(&self) -> Result<Arc<ConnectionBehavior>> {
        self.inner.behavior()
    }

    /// While the surface of this function is a little awkward, this is necessary to provide a
    /// readable error. Authors shouldn't use this directly, they should instead use
    /// `TryFromEnv`.
    pub async fn injector<T: TryFromEnv>(
        &self,
        env: &fho::FhoEnvironment,
    ) -> Result<Arc<dyn Injector>> {
        let strict = env.ffx_command().global.strict;
        let behavior = self.behavior()?;
        match *behavior {
            ConnectionBehavior::DaemonConnector(ref dc) => Ok(dc.clone()),
            _ => {
                if strict {
                    Err(ffx_command_error::user_error!(
                        "ffx-strict doesn't support use of the daemon, which is used to allocate '{}'. This command must either be re-written or you should not use it.",
                        std::any::type_name::<T>()
                    ))
                } else {
                    Err(ffx_command_error::user_error!(
                        "Attempting to use the daemon to allocate '{}', which is not yet supported with {:?}",
                        std::any::type_name::<T>(),
                        behavior
                    ))
                }
            }
        }
    }
}

#[derive(Default)]
pub struct FhoTargetEnvironmentInner {
    /// Defines how to connect to a Fuchsia device. It can be
    /// lazily initialized, and potentially used by multiple threads,
    /// hence the complicated type.
    behavior: Mutex<Option<Arc<ConnectionBehavior>>>,
}

impl FhoTargetEnvironmentInner {
    #[cfg(test)]
    pub fn new_for_test(behavior: ConnectionBehavior) -> Self {
        Self { behavior: Mutex::new(Some(Arc::new(behavior))) }
    }

    /// This attempts to wrap errors around a potential failure in the underlying connection being
    /// used to facilitate FIDL protocols. This should NOT be used by developers, this is intended
    /// to be used outside of the scope of an ffx subtool (outside of the `main` function).
    fn maybe_wrap_connection_errors(&self, err: fho::Error) -> fho::Error {
        if let Ok(behavior) = self.behavior() {
            if let ConnectionBehavior::DirectConnector(ref dc) = *behavior
                && let Some(conn) = dc.get_connection_if_already_established()
            {
                return fho::Error::User(conn.wrap_connection_errors(err.into()));
            }
        }
        err
    }

    /// Initialize either a daemon connection or a direct connection,
    /// depending on how the tool was run. If will be a direct connection
    /// if any of:
    ///   * we are in strict mode
    ///   * the `connectivity.direct=true` config is set (e.g. with "ffx -d")
    pub async fn init_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        if context.is_strict() || context.get_direct_connection_mode() {
            self.init_direct_connection_behavior(context).await
        } else {
            self.init_daemon_connection_behavior(context).await
        }
    }

    /// Explicitly create direct connection behavior.
    pub async fn init_direct_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        log::info!("Initializing ConnectionBehavior::DirectConnector");
        let resolution = Resolution::try_from_env_context(context).await?;
        let behavior = ConnectionBehavior::DirectConnector(Arc::new(resolution));
        self.set_behavior(behavior)
    }

    /// Explicitly create daemon connection behavior, for subtools such as `ffx daemon echo`
    /// which we guarantee will use the daemon, irrespective of the configured connection type.
    /// Returns an error when in strict mode.
    pub async fn init_daemon_connection_behavior(
        &self,
        context: &EnvironmentContext,
    ) -> Result<Arc<ConnectionBehavior>> {
        if context.is_strict() {
            return Err(ffx_command_error::Error::User(anyhow::anyhow!(
                "Daemon connections are not supported in strict mode"
            )));
        }
        let build_info = context.build_info();
        let overnet_injector =
            Injection::initialize_overnet(context.clone(), None, build_info).await?;
        log::info!("Initializing ConnectionBehavior::DaemonConnector");
        let behavior = ConnectionBehavior::DaemonConnector(Arc::new(overnet_injector));
        self.set_behavior(behavior)
    }

    pub fn behavior(&self) -> Result<Arc<ConnectionBehavior>> {
        let b = self.behavior.lock().expect("poisoned behavior lock");
        match *b {
            Some(ref behavior) => Ok(behavior.clone()),
            _ => Err(fho::bug!("Connection behavior is not initialized")),
        }
    }

    fn set_behavior(&self, new_behavior: ConnectionBehavior) -> Result<Arc<ConnectionBehavior>> {
        log::debug!("setting behavior");
        let mut behavior = self.behavior.lock().expect("poisoned behavior lock");
        // If the behavior was set explicitly, e.g. with FhoTool's "#[direct]"
        // attribute, then we don't want to fail if something later gets a
        // different behavior. But we do want to warn, in case it was unintended.
        if let Some(ref b) = *behavior {
            match (b.as_ref(), &new_behavior) {
                (
                    ConnectionBehavior::DaemonConnector(_),
                    ConnectionBehavior::DirectConnector(_),
                )
                | (
                    ConnectionBehavior::DirectConnector(_),
                    ConnectionBehavior::DaemonConnector(_),
                ) => log::debug!("Fho Connection behavior is already set (to {b:?})"),
                _ => (),
            }
            return Ok(b.clone());
        }
        let new_behavior = Arc::new(new_behavior);
        *behavior = Some(new_behavior.clone());
        log::debug!("setting behavior done");
        Ok(new_behavior)
    }

    /// While the surface of this function is a little awkward, this is necessary to provide a
    /// readable error. Authors shouldn't use this directly, they should instead use
    /// `TryFromEnv`.
    pub async fn injector<T: TryFromEnv>(
        &self,
        env: &fho::FhoEnvironment,
    ) -> Result<Arc<dyn Injector>> {
        let strict = env.ffx_command().global.strict;
        let behavior = self.behavior()?;
        match *behavior {
            ConnectionBehavior::DaemonConnector(ref dc) => Ok(dc.clone()),
            _ => {
                if strict {
                    Err(ffx_command_error::user_error!(
                        "ffx-strict doesn't support use of the daemon, which is used to allocate '{}'. This command must either be re-written or you should not use it.",
                        std::any::type_name::<T>()
                    ))
                } else {
                    Err(ffx_command_error::user_error!(
                        "Attempting to use the daemon to allocate '{}', which is not yet supported with {:?}",
                        std::any::type_name::<T>(),
                        behavior
                    ))
                }
            }
        }
    }
}

impl fho::EnvironmentInterface for FhoTargetEnvironment {
    fn wrap_main_errors(&self, err: fho::Error) -> fho::Error {
        self.maybe_wrap_connection_errors(err)
    }
}
pub fn target_interface(env: &fho::FhoEnvironment) -> FhoTargetEnvironment {
    if env.get_interface::<FhoTargetEnvironment>().is_none() {
        let target_interface = FhoTargetEnvironment::default();
        env.set_interface(target_interface);
    }
    env.get_interface::<FhoTargetEnvironment>().expect("No target interface in FhoEnvironment??")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ffx_config::environment::ExecutableKind;
    use ffx_config::{ConfigMap, test_env};

    #[fuchsia::test]
    async fn test_connection_behavior_correct_in_strict() {
        let runtime_args =
            serde_json::json!({"target": {"default" : "127.0.0.1"}}).as_object().unwrap().clone();
        let ctx = EnvironmentContext::strict(ExecutableKind::Test, runtime_args).unwrap();
        let target_env = FhoTargetEnvironment::default();
        let behavior = target_env.init_connection_behavior(&ctx).await.unwrap();
        assert!(matches!(*behavior, ConnectionBehavior::DirectConnector(_)));
    }

    #[fuchsia::test]
    async fn test_connection_behavior_correct_in_non_strict() {
        let env = test_env().build().unwrap();
        let target_env = FhoTargetEnvironment::default();
        let behavior = target_env.init_connection_behavior(&env.context).await.unwrap();
        assert!(matches!(*behavior, ConnectionBehavior::DaemonConnector(_)));
    }

    #[fuchsia::test]
    async fn test_daemon_connection_behavior() {
        let env = test_env().build().unwrap();
        let target_env = FhoTargetEnvironment::default();
        let behavior = target_env.init_daemon_connection_behavior(&env.context).await.unwrap();
        assert!(matches!(*behavior, ConnectionBehavior::DaemonConnector(_)));
    }

    #[fuchsia::test]
    async fn test_daemon_connection_behavior_fails_in_strict() {
        let ctx =
            EnvironmentContext::strict(ExecutableKind::Test, ConfigMap::new()).expect("strict env");
        let target_env = FhoTargetEnvironment::default();
        assert!(matches!(target_env.init_daemon_connection_behavior(&ctx).await, Err(_)));
    }
    #[fuchsia::test]
    async fn test_direct_connection_behavior() {
        let env = test_env()
            .runtime_config("connectivity.direct", true)
            .runtime_config("target.default", "127.0.0.1")
            .build()
            .unwrap();
        let target_env = FhoTargetEnvironment::default();
        let behavior = target_env.init_connection_behavior(&env.context).await.unwrap();
        assert!(matches!(*behavior, ConnectionBehavior::DirectConnector(_)));
    }

    #[fuchsia::test]
    fn set_behavior_succeeds_when_called_twice() {
        let beh1 = ConnectionBehavior::DirectConnector(Arc::new(ffx_target::Resolution::mock(
            || unreachable!(),
        )));
        let fho_env = FhoTargetEnvironmentInner::new_for_test(beh1);
        let beh2 = ConnectionBehavior::DirectConnector(Arc::new(ffx_target::Resolution::mock(
            || unreachable!(),
        )));
        let res = fho_env.set_behavior(beh2);
        assert!(matches!(res, Ok(_)));
    }

    struct FakeInjector;
    #[async_trait(?Send)]
    impl Injector for FakeInjector {
        async fn daemon_factory(
            &self,
        ) -> anyhow::Result<fidl_fuchsia_developer_ffx::DaemonProxy, ffx_core::FfxInjectorError>
        {
            unreachable!()
        }
        async fn daemon_factory_force_autostart(
            &self,
        ) -> anyhow::Result<fidl_fuchsia_developer_ffx::DaemonProxy, ffx_core::FfxInjectorError>
        {
            unreachable!()
        }
        async fn remote_factory(
            &self,
        ) -> anyhow::Result<fidl_fuchsia_developer_remotecontrol::RemoteControlProxy> {
            unreachable!()
        }
        async fn remote_factory_fdomain(
            &self,
        ) -> anyhow::Result<fdomain_fuchsia_developer_remotecontrol::RemoteControlProxy> {
            unreachable!()
        }
        async fn target_factory(&self) -> anyhow::Result<ffx_target::TargetProxy> {
            unreachable!()
        }
    }

    #[fuchsia::test]
    fn set_behavior_can_be_called_twice_incompatiblty() {
        let beh1 = ConnectionBehavior::DirectConnector(Arc::new(ffx_target::Resolution::mock(
            || unreachable!(),
        )));
        let fho_env = FhoTargetEnvironmentInner::new_for_test(beh1);
        let fake_injector = FakeInjector;
        let beh2 = ConnectionBehavior::DaemonConnector(Arc::new(fake_injector));
        let res = fho_env.set_behavior(beh2);
        assert!(matches!(res, Ok(_)));
    }
}
