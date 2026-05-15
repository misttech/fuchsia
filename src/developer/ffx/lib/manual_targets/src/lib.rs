// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_lock::Mutex;
use async_trait::async_trait;

use ffx_config::{ConfigLevel, EnvironmentContext};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(thiserror::Error, Debug)]
pub enum ManualTargetsError {
    #[error("Config error: {0}")]
    Config(#[from] ffx_config::api::ConfigError),

    #[error("Mock targets value is missing")]
    MockValueMissing,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Interface factory error: {0}")]
    InterfaceFactory(#[from] ffx_fastboot_interface::interface_factory::InterfaceFactoryError),
}

pub mod watcher;

#[cfg(test)]
pub(crate) const MANUAL_TARGETS: &'static str = "targets.manual";
#[cfg(not(test))]
/// Configuration key containing the list of manual targets
const MANUAL_TARGETS: &'static str = "targets.manual";

#[async_trait(?Send)]
pub trait ManualTargets: Sync {
    async fn storage_set(&self, targets: Value) -> Result<(), ManualTargetsError>;
    async fn storage_get(&self) -> Result<Value, ManualTargetsError>;

    async fn get(&self) -> Result<Value, ManualTargetsError> {
        self.storage_get().await
    }

    async fn get_or_default(&self) -> Map<String, Value> {
        self.get()
            .await
            .unwrap_or_else(|_| Value::default())
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    async fn add(&self, target: String) -> Result<(), ManualTargetsError> {
        let mut targets = self.get_or_default().await;
        // We always insert None so that we retain backwards-compatibility for manual targets,
        // which previously were stored as a map of addr->expiration-time
        targets.insert(target, json!(None::<Option<u64>>));
        self.storage_set(targets.into()).await
    }

    async fn remove(&self, target: String) -> Result<(), ManualTargetsError> {
        let mut targets = self.get_or_default().await;
        targets.remove(&target);
        self.storage_set(targets.into()).await
    }
}

pub struct Config {
    context: EnvironmentContext,
}

impl Config {
    pub fn new_from_context(context: &EnvironmentContext) -> Self {
        Self { context: context.clone() }
    }
}

#[async_trait(?Send)]
impl ManualTargets for Config {
    async fn storage_get(&self) -> Result<Value, ManualTargetsError> {
        self.context
            .query(MANUAL_TARGETS)
            .level(Some(ConfigLevel::User))
            .build()
            .get(&self.context)
            .map_err(ManualTargetsError::Config)
    }

    async fn storage_set(&self, targets: Value) -> Result<(), ManualTargetsError> {
        self.context
            .query(MANUAL_TARGETS)
            .level(Some(ConfigLevel::User))
            .build()
            .set(&self.context, targets.into())
            .map_err(ManualTargetsError::Config)
    }
}

#[derive(Default)]
pub struct Mock {
    targets: Mutex<Option<Value>>,
    set_count: AtomicUsize,
}

#[async_trait(?Send)]
impl ManualTargets for Mock {
    async fn storage_get(&self) -> Result<Value, ManualTargetsError> {
        self.targets.lock().await.clone().ok_or(ManualTargetsError::MockValueMissing)
    }

    async fn storage_set(&self, targets: Value) -> Result<(), ManualTargetsError> {
        let _ = self
            .set_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| {
                Some(targets.as_object().unwrap().len())
            })
            .expect("Couldn't update target count for Mock.");
        self.targets.lock().await.replace(targets);
        Ok(())
    }
}

impl Mock {
    #[cfg(test)]
    pub fn new(targets: Map<String, Value>) -> Self {
        Self { targets: Mutex::new(Some(Value::from(targets))), ..Self::default() }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serial_test::serial;

    mod real_impl {
        use super::*;
        use serde_json::json;
        use serial_test::serial;

        #[fuchsia::test]
        #[serial]
        async fn test_get_manual_targets() {
            let env = ffx_config::test_init().unwrap();

            env.context
                .query(MANUAL_TARGETS)
                .level(Some(ConfigLevel::User))
                .build()
                .set(&env.context, json!({"127.0.0.1:8022": 0, "127.0.0.1:8023": 12345}))
                .unwrap();

            let mt = Config::new_from_context(&env.context);
            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
            assert!(targets.contains_key("127.0.0.1:8023"));
        }

        #[fuchsia::test]
        #[serial]
        async fn test_add_manual_target() {
            let env = ffx_config::test_init().unwrap();

            let mt = Config::new_from_context(&env.context);
            mt.add("127.0.0.1:8022".to_string()).await.unwrap();
            // duplicate additions are ignored
            mt.add("127.0.0.1:8022".to_string()).await.unwrap();

            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
        }

        #[fuchsia::test]
        #[serial]
        async fn test_remove_manual_target() {
            let env = ffx_config::test_init().unwrap();

            env.context
                .query(MANUAL_TARGETS)
                .level(Some(ConfigLevel::User))
                .build()
                .set(&env.context, json!({"127.0.0.1:8022": 0, "127.0.0.1:8023": 0}))
                .unwrap();

            let mt = Config::new_from_context(&env.context);
            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
            assert!(targets.contains_key("127.0.0.1:8023"));

            mt.remove("127.0.0.1:8022".to_string()).await.unwrap();
            mt.remove("127.0.0.1:8023".to_string()).await.unwrap();

            let targets = mt.get_or_default().await;
            assert_eq!(targets, Map::<String, Value>::new());
        }
    }

    mod mock_impl {
        use super::*;

        #[fuchsia::test]
        async fn test_new() {
            let mut map = Map::new();
            map.insert("127.0.0.1:8022".to_string(), json!(0));
            let mt = Mock::new(map);
            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
        }

        #[fuchsia::test]
        async fn test_default() {
            let mt = Mock::default();
            assert_eq!(mt.get_or_default().await, Map::<String, Value>::new());
        }

        #[fuchsia::test]
        async fn test_get_manual_targets() {
            let mut map = Map::new();
            map.insert("127.0.0.1:8022".to_string(), json!(0));
            map.insert("127.0.0.1:8023".to_string(), json!(0));
            let mt = Mock::new(map);
            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
            assert!(targets.contains_key("127.0.0.1:8023"));
        }

        #[fuchsia::test]
        async fn test_add_manual_target() {
            let mt = Mock::default();
            mt.add("127.0.0.1:8022".to_string()).await.unwrap();
            // duplicate additions are ignored
            mt.add("127.0.0.1:8022".to_string()).await.unwrap();

            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));
        }

        #[fuchsia::test]
        async fn test_remove_manual_target() {
            let mut map = Map::new();
            map.insert("127.0.0.1:8022".to_string(), json!(0));
            let mt = Mock::new(map);
            let value = mt.get().await.unwrap();
            let targets = value.as_object().unwrap();
            assert!(targets.contains_key("127.0.0.1:8022"));

            mt.remove("127.0.0.1:8022".to_string()).await.unwrap();

            let targets = mt.get_or_default().await;
            assert!(targets.is_empty());
        }
    }

    #[fuchsia::test]
    #[serial]
    async fn test_repeated_adds_do_not_rewrite_storage() {
        let mt = Mock::new(Map::new());
        mt.add("127.0.0.1:8022".to_string()).await.unwrap();
        assert_eq!(mt.set_count.load(Ordering::SeqCst), 1);
        mt.add("127.0.0.1:8022".to_string()).await.unwrap();
        assert_eq!(mt.set_count.load(Ordering::SeqCst), 1);
    }
}
