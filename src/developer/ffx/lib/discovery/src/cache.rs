// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::CacheError;
use chrono::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use tempfile::NamedTempFile;

pub type Result<T> = std::result::Result<T, crate::error::CacheError>;

use crate::TargetHandle;

const CACHE_VERSION: u32 = 1;
const CACHE_TTL_SECONDS: i64 = 60;

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    version: u32,
    expires: DateTime<Utc>,
    pub(crate) targets: Vec<TargetHandle>,
}

impl Cache {
    pub(crate) fn new(targets: Vec<TargetHandle>) -> Self {
        Self {
            version: CACHE_VERSION,
            expires: Utc::now() + chrono::Duration::seconds(CACHE_TTL_SECONDS),
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
        let Some(dir) = path.parent() else {
            return Err(CacheError::BadLocation { path: path.to_path_buf() });
        };
        let mut temp_file = NamedTempFile::new_in(dir)
            .map_err(|err| CacheError::CreateFile { path: path.to_path_buf(), err })?;
        let writer = BufWriter::new(&mut temp_file);
        serde_json::to_writer(writer, self)
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
        log::debug!("Cache {path:?} saved at {}", self.expires.format("%Y-%m-%d %H:%M:%S"));
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_expires(&mut self, expires: DateTime<Utc>) {
        self.expires = expires;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TargetState;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn create_test_cache(targets: Vec<TargetHandle>) -> Cache {
        Cache::new(targets)
    }

    fn create_test_handle(name: &str) -> TargetHandle {
        TargetHandle {
            node_name: Some(name.to_string()),
            state: TargetState::Unknown,
            manual: false,
        }
    }

    #[fuchsia::test]
    fn test_save_and_load_successfully() -> Result<()> {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let handle = create_test_handle("test-target");
        let cache_to_save = create_test_cache(vec![handle.clone()]);

        cache_to_save.save(&cache_path)?;

        let loaded_cache = Cache::load(&cache_path)?;

        assert_eq!(loaded_cache.version, CACHE_VERSION);
        assert_eq!(loaded_cache.targets.len(), 1);
        assert_eq!(loaded_cache.targets[0], handle);
        assert!(loaded_cache.expires > Utc::now());

        Ok(())
    }

    #[fuchsia::test]
    fn test_load_expired_cache() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let handle = create_test_handle("test-target");
        let mut cache = create_test_cache(vec![handle]);
        cache.expires = Utc::now() - chrono::Duration::seconds(1);

        cache.save(&cache_path).unwrap();

        let result = Cache::load(&cache_path);
        assert!(matches!(result, Err(CacheError::Expired(_))));
    }

    #[fuchsia::test]
    fn test_load_bad_version() {
        let dir = tempdir().unwrap();
        let cache_path = dir.path().join("cache.json");
        let handle = create_test_handle("test-target");
        let mut cache = create_test_cache(vec![handle]);
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
        let handle = create_test_handle("test-target");
        let cache_to_save = create_test_cache(vec![handle]);

        let result = cache_to_save.save(&cache_path);
        assert!(matches!(result, Err(CacheError::CreateFile { .. })));

        // Restore write permissions so the tempdir can be cleaned up.
        let mut perms = fs::metadata(readonly_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(readonly_dir, perms).unwrap();
    }
}
