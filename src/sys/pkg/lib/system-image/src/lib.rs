// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(clippy::let_unit_value)]

mod anchored_packages;
mod cache_packages;
mod errors;
mod path_hash_mapping;
mod system_image;

pub use crate::anchored_packages::AnchoredPackages;
pub use crate::cache_packages::CachePackages;
pub use crate::errors::{
    AllowListError, CachePackagesInitError, PathHashMappingError, StaticPackagesInitError,
};
pub use crate::path_hash_mapping::{Bootfs, PathHashMapping, StaticPackages};
pub use crate::system_image::{ExecutabilityRestrictions, SystemImage};

static PKGFS_BOOT_ARG_KEY: &str = "zircon.system.pkgfs.cmd";
static PKGFS_BOOT_ARG_VALUE_PREFIX: &str = "bin/pkgsvr+";

pub fn get_system_image_hash(
    system_image_hash: &str,
) -> Result<fuchsia_hash::Hash, SystemImageHashError> {
    let hash = system_image_hash
        .strip_prefix(PKGFS_BOOT_ARG_VALUE_PREFIX)
        .ok_or_else(|| SystemImageHashError::BadPrefix(system_image_hash.to_string()))?;
    hash.parse().map_err(SystemImageHashError::BadHash)
}

#[derive(Debug, thiserror::Error)]
pub enum SystemImageHashError {
    #[error(
        "boot arg for key {} does not start with {}: {:?}",
        PKGFS_BOOT_ARG_KEY,
        PKGFS_BOOT_ARG_VALUE_PREFIX,
        .0
    )]
    BadPrefix(String),

    #[error("boot arg for key {} has invalid hash {:?}", PKGFS_BOOT_ARG_KEY, .0)]
    BadHash(#[source] fuchsia_hash::ParseHashError),
}

#[cfg(test)]
mod test_get_system_image_hash {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn bad_prefix() {
        assert_matches!(
            get_system_image_hash("bad-prefix"),
            Err(SystemImageHashError::BadPrefix(prefix)) if prefix == "bad-prefix"
        );
    }

    #[test]
    fn bad_hash() {
        assert_matches!(
            get_system_image_hash("bin/pkgsvr+bad-hash"),
            Err(SystemImageHashError::BadHash(_))
        );
    }

    #[test]
    fn success() {
        assert_eq!(
            get_system_image_hash(
                "bin/pkgsvr+0000000000000000000000000000000000000000000000000000000000000000"
            )
            .unwrap(),
            fuchsia_hash::Hash::from([0; 32])
        );
    }
}
