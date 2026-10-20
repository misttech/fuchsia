// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::TargetInfo;
use crate::info::TargetState;
use addr::TargetAddr;
use chrono::prelude::*;
use discovery::query::TargetInfoQuery;
use ffx_config::EnvironmentContext;
use ffx_config::keys::{DISCOVERY_CACHE_DIR_CONFIG, REGISTERED_TARGETS_CONFIG};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;
use thiserror::Error;

const CACHE_FILE_NAME: &str = "ffx-target-info.json";
const CACHE_VERSION: u32 = 2;
const CACHE_TTL_SECONDS: u64 = 60;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

type Result<T> = std::result::Result<T, CacheError>;

/// Errors returned when reading or writing the target discovery cache or registered targets file.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("bad location of cache file at {path:?}")]
    BadLocation { path: PathBuf },

    #[error("opening cache file at {path:?}")]
    OpenFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("creating cache file at {path:?}")]
    CreateFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("deserializing cache from {path:?}")]
    Deserialize {
        path: PathBuf,
        #[source]
        err: serde_json::Error,
    },

    #[error("serializing cache to {path:?}")]
    Serialize {
        path: PathBuf,
        #[source]
        err: serde_json::Error,
    },

    #[error("bad cache version: {0}")]
    BadVersion(u32),

    #[error("cache expired at {0}")]
    Expired(chrono::DateTime<chrono::Utc>),

    #[error("could not rename cache file from {from:?} to {to:?}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("could not get discovery cache file path")]
    NoCacheFile,

    #[error("could not get shared_data path: {0}")]
    SharedData(String),

    #[error("cannot stat cache file at {path:?}")]
    StatFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("cannot remove cache file at {path:?}")]
    RemoveFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },
}

/// Minimal target device record stored in `$SHARED_DATA/targets.json`.
///
/// This record stores only the fields necessary for resolving symbolic target queries
/// (`nodename`, `id`, and `address`) using a plain string transport address so
/// external test orchestrators can read or author the file directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredTarget {
    /// Target nodename, if known.
    #[serde(default)]
    pub nodename: Option<String>,
    /// Target identifier (hardware or virtual serial number), if known.
    #[serde(default)]
    pub id: Option<String>,
    /// Primary transport address for the target, formatted as a standard string.
    #[serde(
        serialize_with = "serialize_target_addr",
        deserialize_with = "deserialize_target_addr"
    )]
    pub address: TargetAddr,
}

impl RegisteredTarget {
    /// Constructs a [`RegisteredTarget`] from a [`TargetInfo`] if it is in a connectable
    /// state and has at least one transport address, selecting the highest-priority address.
    pub fn from_target_info(info: &TargetInfo) -> Option<Self> {
        if !matches!(info.target_state, TargetState::Product | TargetState::Unknown) {
            return None;
        }
        let address = info.addresses.iter().cloned().min_by(TargetAddr::compare_by_priority)?;
        Some(Self { nodename: info.nodename.clone(), id: info.serial_number.clone(), address })
    }
}

impl From<RegisteredTarget> for TargetInfo {
    fn from(registered: RegisteredTarget) -> Self {
        Self {
            nodename: registered.nodename,
            addresses: vec![registered.address],
            target_state: TargetState::Product,
            serial_number: registered.id,
            ..Default::default()
        }
    }
}

// Serializes a TargetAddr value as its standard string representation.
fn serialize_target_addr<S>(
    address: &TargetAddr,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: Serializer,
{
    address.optional_port_str().serialize(serializer)
}

// Deserializes a target address string into a TargetAddr value.
fn deserialize_target_addr<'de, D>(deserializer: D) -> std::result::Result<TargetAddr, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
}

/// Returns the path to the registered targets file (`target.registered_targets`,
/// which defaults to `$SHARED_DATA/targets.json`) for the given environment context.
///
/// # Errors
///
/// Returns [`CacheError::SharedData`] if the configuration path cannot be resolved
/// (such as in strict mode when neither `-c shared_data=<dir>` nor
/// `-c target.registered_targets=<path>` is specified).
pub fn targets_file_path(context: &EnvironmentContext) -> Result<PathBuf> {
    context
        .get::<PathBuf, _>(REGISTERED_TARGETS_CONFIG)
        .map_err(|e| CacheError::SharedData(e.to_string()))
}

/// Atomically writes discovered connectable targets to `$SHARED_DATA/targets.json`
/// and returns the absolute path of the written file.
///
/// # Errors
///
/// Returns an error if the `shared_data` directory cannot be resolved or if writing and
/// persisting the file fails.
pub fn register_targets(
    context: &EnvironmentContext,
    infos: &[TargetInfo],
) -> std::result::Result<PathBuf, crate::FfxTargetCrateError> {
    let raw_path = targets_file_path(context)?;
    let targets_file = std::path::absolute(&raw_path)
        .map_err(|err| CacheError::CreateFile { path: raw_path, err })?;
    let Some(dir) = targets_file.parent() else {
        return Err(CacheError::BadLocation { path: targets_file }.into());
    };
    fs::create_dir_all(dir)
        .map_err(|err| CacheError::CreateFile { path: targets_file.clone(), err })?;
    let registered: Vec<RegisteredTarget> =
        infos.iter().filter_map(RegisteredTarget::from_target_info).collect();
    save_json_atomically(&registered, &targets_file)?;
    Ok(targets_file)
}

// Loads registered targets from `$SHARED_DATA/targets.json` and converts them into TargetInfo records.
pub(crate) fn load_registered_targets(path: &Path) -> Result<Vec<TargetInfo>> {
    let file = fs::File::open(path)
        .map_err(|err| CacheError::OpenFile { path: path.to_path_buf(), err })?;
    let reader = BufReader::new(file);
    let registered: Vec<RegisteredTarget> = serde_json::from_reader(reader)
        .map_err(|err| CacheError::Deserialize { path: path.to_path_buf(), err })?;
    Ok(registered.into_iter().map(TargetInfo::from).collect())
}

// Atomically writes a serializable value as JSON to `path` via a temporary file in the same directory.
fn save_json_atomically<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let Some(dir) = path.parent() else {
        return Err(CacheError::BadLocation { path: path.to_path_buf() });
    };
    let mut temp_file = NamedTempFile::new_in(dir)
        .map_err(|err| CacheError::CreateFile { path: path.to_path_buf(), err })?;
    let writer = BufWriter::new(&mut temp_file);
    serde_json::to_writer(writer, value)
        .map_err(|err| CacheError::Serialize { path: path.to_path_buf(), err })?;
    temp_file
        .as_file_mut()
        .sync_all()
        .map_err(|err| CacheError::CreateFile { path: path.to_path_buf(), err })?;
    temp_file.persist(path).map_err(|e| CacheError::Rename {
        from: e.file.path().to_path_buf(),
        to: path.to_path_buf(),
        err: e.error,
    })?;
    Ok(())
}

/// Create the target discovery cache file. The directory containing the file
/// must already exist. This will list the current targets, but will not try to
/// make an RCS connection to them.
pub async fn create_target_cache(
    context: &EnvironmentContext,
) -> std::result::Result<Vec<TargetInfo>, crate::FfxTargetCrateError> {
    let Some(cache_file) = get_discovery_cache_file(context) else {
        return Err(CacheError::NoCacheFile.into());
    };
    let infos =
        crate::list::list_targets(context, TargetInfoQuery::First, true, true, true).await?;
    // The cache is used to quickly find targets matching a query. With that
    // in mind, we are storing more than we need to (since queries are based
    // only on the name/serial/addrs).  We could just store the subset of the
    // information that we need, but it doesn't save time since we have to do
    // list_targets() regardless. Keeping the extra information could allow us
    // to add other queries in the future, such as by board.
    // Note that while the cache is only used to "resolve" queries (which is a
    // term used to resolve to a _product_), we are storing enough information
    // to match against non-product targets as well. We do not because the
    // assumption is that subtools (e.g. flash) that want to find non-product
    // targets generally avoid the cache, since they want to get the most
    // up-to-date state. But this may change, which is why the cache is not
    // explicitly associated with a "Resolution".
    let cache = Cache::new(infos.clone());
    cache.save(&cache_file)?;
    Ok(infos)
}

/// Remove the target cache file, if it exists.
pub fn remove_target_cache(
    context: &EnvironmentContext,
) -> std::result::Result<(), crate::FfxTargetCrateError> {
    let Some(cache_file) = get_discovery_cache_file(context) else {
        return Ok(());
    };
    if std::fs::exists(&cache_file)
        .map_err(|e| CacheError::StatFile { path: cache_file.clone(), err: e })?
    {
        std::fs::remove_file(&cache_file)
            .map_err(|e| CacheError::RemoveFile { path: cache_file.clone(), err: e })?;
    }
    Ok(())
}

/// Directory containing the discovery cache file
pub fn get_discovery_cache_dir(context: &EnvironmentContext) -> Option<PathBuf> {
    let path_s: Option<String> = match context.get(DISCOVERY_CACHE_DIR_CONFIG) {
        Ok(opath) => opath,
        Err(e) => {
            log::debug!("Could not get {DISCOVERY_CACHE_DIR_CONFIG}: {e}");
            None
        }
    };
    path_s.map(|s| s.into())
}

/// Path to discovery cache file, if any
pub fn get_discovery_cache_file(context: &EnvironmentContext) -> Option<PathBuf> {
    let cache_path = get_discovery_cache_dir(context);
    cache_path.map(|mut d| {
        d.push(CACHE_FILE_NAME);
        d
    })
}

/// Return the time to wait before updating the cache. Because writing the cache itself takes
/// some time (due to the need to wait for discovery to complete), the recheck time is
/// less than the cache TTL.
pub fn get_discovery_cache_recheck_time() -> Duration {
    Duration::from_secs(CACHE_TTL_SECONDS) - DEFAULT_TIMEOUT
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    version: u32,
    expires: DateTime<Utc>,
    pub(crate) targets: Vec<TargetInfo>,
}

impl Cache {
    pub(crate) fn new(targets: Vec<TargetInfo>) -> Self {
        Self {
            version: CACHE_VERSION,
            expires: Utc::now() + chrono::Duration::seconds(CACHE_TTL_SECONDS as i64),
            targets,
        }
    }
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let file = fs::File::open(path)
            .map_err(|err| CacheError::OpenFile { path: path.to_path_buf(), err })?;
        let reader = BufReader::new(file);
        let cache: Cache = serde_json::from_reader(reader)
            .map_err(|err| CacheError::Deserialize { path: path.to_path_buf(), err })?;
        if cache.version != CACHE_VERSION {
            return Err(CacheError::BadVersion(cache.version));
        }
        if Utc::now() > cache.expires {
            log::debug!(
                "cache at {path:?} expired at {}",
                cache.expires.format("%Y-%m-%d %H:%M:%S")
            );
            return Err(CacheError::Expired(cache.expires));
        }
        Ok(cache)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        save_json_atomically(self, path)?;
        log::debug!("Cache {path:?} saved at {}", self.expires.format("%Y-%m-%d %H:%M:%S"));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn _set_expires(&mut self, expires: DateTime<Utc>) {
        self.expires = expires;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_config::environment::ExecutableKind;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn create_test_cache(targets: Vec<TargetInfo>) -> Cache {
        Cache::new(targets)
    }

    fn create_test_info(name: &str) -> TargetInfo {
        TargetInfo { nodename: Some(name.to_string()), ..TargetInfo::default() }
    }

    #[fuchsia::test]
    fn test_save_and_load_successfully() -> Result<()> {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let info = create_test_info("test-target");
        let cache_to_save = create_test_cache(vec![info.clone()]);

        cache_to_save.save(&cache_path)?;

        let loaded_cache = Cache::load(&cache_path)?;

        assert_eq!(loaded_cache.version, CACHE_VERSION);
        assert_eq!(loaded_cache.targets.len(), 1);
        assert_eq!(loaded_cache.targets[0], info);
        assert!(loaded_cache.expires > Utc::now());

        Ok(())
    }

    #[fuchsia::test]
    fn test_load_expired_cache() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let info = create_test_info("test-target");
        let mut cache = create_test_cache(vec![info]);
        cache.expires = Utc::now() - chrono::Duration::seconds(1);

        cache.save(&cache_path).unwrap();

        let result = Cache::load(&cache_path);
        assert!(matches!(result, Err(CacheError::Expired(_))));
    }

    #[fuchsia::test]
    fn test_load_bad_version() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let info = create_test_info("test-target");
        let mut cache = create_test_cache(vec![info]);
        cache.version = CACHE_VERSION + 1;

        cache.save(&cache_path).unwrap();

        let result = Cache::load(&cache_path);
        assert!(matches!(result, Err(CacheError::BadVersion(_))));
    }

    #[fuchsia::test]
    fn test_load_file_not_found() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("nonexistent.json");
        let result = Cache::load(&cache_path);
        assert!(matches!(result, Err(CacheError::OpenFile { .. })));
    }

    #[fuchsia::test]
    fn test_load_corrupt_json() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("corrupt.json");
        let mut file = fs::File::create(&cache_path).unwrap();
        writeln!(file, "{{{{ not valid json }}").unwrap();

        let result = Cache::load(&cache_path);
        assert!(matches!(result, Err(CacheError::Deserialize { .. })));
    }

    #[fuchsia::test]
    fn test_save_permission_denied() {
        let dir = tempdir().unwrap();
        let readonly_dir = dir.path();
        let mut perms = fs::metadata(readonly_dir).unwrap().permissions();
        perms.set_mode(0o555); // Read and execute, but not write
        fs::set_permissions(readonly_dir, perms).unwrap();

        let cache_path = readonly_dir.join("cache.json");
        let info = create_test_info("test-target");
        let cache_to_save = create_test_cache(vec![info]);

        let result = cache_to_save.save(&cache_path);
        assert!(matches!(result, Err(CacheError::CreateFile { .. })));

        // Restore write permissions so the tempdir can be cleaned up.
        let mut perms = fs::metadata(readonly_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(readonly_dir, perms).unwrap();
    }

    #[fuchsia::test]
    async fn test_get_discovery_cache_dir() {
        let cache_dir = "/tmp/cache";
        let test_env = ffx_config::test_env()
            .user_config(DISCOVERY_CACHE_DIR_CONFIG, cache_dir.to_string())
            .build()
            .unwrap();

        let result = get_discovery_cache_dir(&test_env.context);
        assert_eq!(result, Some(PathBuf::from(cache_dir)));
    }

    #[fuchsia::test]
    async fn test_get_discovery_cache_dir_strict() {
        let context =
            EnvironmentContext::strict(ExecutableKind::Test, ffx_config::ConfigMap::new()).unwrap();
        let result = get_discovery_cache_dir(&context);
        assert!(result.is_none(), "Expected none, got {result:?}");
    }

    #[fuchsia::test]
    fn test_get_discovery_cache_dir_strict_exists() {
        let cache_dir = "/tmp/foo/bar";
        let config_map =
            [("target".to_owned(), serde_json::json!({"discovery_cache_dir": "/tmp/foo/bar"}))]
                .into_iter()
                .collect();
        let context = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();

        let result = get_discovery_cache_dir(&context);
        assert_eq!(result, Some(PathBuf::from(cache_dir)));
    }

    #[fuchsia::test]
    async fn test_register_and_load_registered_targets() {
        let dir = tempdir().unwrap();
        let shared_dir = dir.path().join("shared");
        let test_env = ffx_config::test_env()
            .runtime_config("shared_data", shared_dir.to_str().unwrap())
            .build()
            .unwrap();

        let addr_v4: TargetAddr = "192.168.1.50:8022".parse().unwrap();
        let product_info = TargetInfo {
            nodename: Some("fuchsia-node".to_string()),
            serial_number: Some("04012C12D45E".to_string()),
            addresses: vec![addr_v4.clone(), TargetAddr::UsbCtx(3)],
            target_state: TargetState::Product,
            ..Default::default()
        };
        let fastboot_info = TargetInfo {
            nodename: Some("fastboot-node".to_string()),
            serial_number: Some("FB123".to_string()),
            addresses: vec![addr_v4],
            target_state: TargetState::Fastboot,
            ..Default::default()
        };

        register_targets(&test_env.context, &[product_info, fastboot_info]).unwrap();

        let targets_file = targets_file_path(&test_env.context).unwrap();
        assert_eq!(targets_file, shared_dir.join("targets.json"));
        let loaded = load_registered_targets(&targets_file).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].nodename.as_deref(), Some("fuchsia-node"));
        assert_eq!(loaded[0].serial_number.as_deref(), Some("04012C12D45E"));
        assert_eq!(loaded[0].addresses, vec![TargetAddr::UsbCtx(3)]);
        assert_eq!(loaded[0].target_state, TargetState::Product);
    }

    #[fuchsia::test]
    fn test_load_registered_targets_minimal_infra_json() {
        let dir = tempdir().unwrap();
        let targets_file = dir.path().join("targets.json");
        let raw_json = r#"[
            {
                "id": "04012C12D45E",
                "address": "192.168.1.50:22"
            },
            {
                "nodename": "only-name",
                "address": "10.0.0.2"
            },
            {
                "id": "USB_DEV",
                "address": "usb:cid:4"
            }
        ]"#;
        fs::write(&targets_file, raw_json).unwrap();

        let loaded = load_registered_targets(&targets_file).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].nodename, None);
        assert_eq!(loaded[0].serial_number.as_deref(), Some("04012C12D45E"));
        assert_eq!(loaded[0].addresses, vec!["192.168.1.50:22".parse::<TargetAddr>().unwrap()]);
        assert_eq!(loaded[1].nodename.as_deref(), Some("only-name"));
        assert_eq!(loaded[1].serial_number, None);
        assert_eq!(loaded[1].addresses, vec!["10.0.0.2:0".parse::<TargetAddr>().unwrap()]);
        assert_eq!(loaded[2].serial_number.as_deref(), Some("USB_DEV"));
        assert_eq!(loaded[2].addresses, vec![TargetAddr::UsbCtx(4)]);
    }

    #[fuchsia::test]
    async fn test_targets_file_path_strict_and_custom_config() {
        let strict_ctx =
            EnvironmentContext::strict(ExecutableKind::Test, ffx_config::ConfigMap::new()).unwrap();
        assert!(matches!(targets_file_path(&strict_ctx), Err(CacheError::SharedData(_))));

        let config_map = [("shared_data".to_owned(), serde_json::json!("/tmp/strict_shared"))]
            .into_iter()
            .collect();
        let context_with_sd = EnvironmentContext::strict(ExecutableKind::Test, config_map).unwrap();
        assert_eq!(
            targets_file_path(&context_with_sd).unwrap(),
            PathBuf::from("/tmp/strict_shared/targets.json")
        );

        let custom_map = [(
            "target".to_owned(),
            serde_json::json!({"registered_targets": "/tmp/custom/my_targets.json"}),
        )]
        .into_iter()
        .collect();
        let context_with_custom =
            EnvironmentContext::strict(ExecutableKind::Test, custom_map).unwrap();
        assert_eq!(
            targets_file_path(&context_with_custom).unwrap(),
            PathBuf::from("/tmp/custom/my_targets.json")
        );
    }
}
