// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use thiserror::Error;

/// Errors returned when reading or writing a JSON-backed target store file on disk.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("bad location of target store file at {path:?}")]
    BadLocation { path: PathBuf },

    #[error("opening target store file at {path:?}")]
    OpenFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("creating target store file at {path:?}")]
    CreateFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("deserializing target store from {path:?}")]
    Deserialize {
        path: PathBuf,
        #[source]
        err: serde_json::Error,
    },

    #[error("serializing target store to {path:?}")]
    Serialize {
        path: PathBuf,
        #[source]
        err: serde_json::Error,
    },

    #[error("could not rename target store file from {from:?} to {to:?}")]
    Rename {
        from: PathBuf,
        to: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("cannot stat target store file at {path:?}")]
    StatFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("cannot remove target store file at {path:?}")]
    RemoveFile {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },
}

impl StoreError {
    /// Returns `true` if the error was caused by the store file not existing on disk.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::OpenFile { err, .. } if err.kind() == std::io::ErrorKind::NotFound)
    }
}

// Reads and deserializes a JSON value from `path`.
pub(crate) fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let file = fs::File::open(path)
        .map_err(|err| StoreError::OpenFile { path: path.to_path_buf(), err })?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader)
        .map_err(|err| StoreError::Deserialize { path: path.to_path_buf(), err })
}

// Atomically writes a serializable value as JSON to `path` via a temporary file in the same directory.
pub(crate) fn save_json_atomically<T: Serialize>(value: &T, path: &Path) -> Result<(), StoreError> {
    let Some(dir) = path.parent() else {
        return Err(StoreError::BadLocation { path: path.to_path_buf() });
    };
    let mut temp_file = NamedTempFile::new_in(dir)
        .map_err(|err| StoreError::CreateFile { path: path.to_path_buf(), err })?;
    let writer = BufWriter::new(&mut temp_file);
    serde_json::to_writer(writer, value)
        .map_err(|err| StoreError::Serialize { path: path.to_path_buf(), err })?;
    temp_file
        .as_file_mut()
        .sync_all()
        .map_err(|err| StoreError::CreateFile { path: path.to_path_buf(), err })?;
    temp_file.persist(path).map_err(|e| StoreError::Rename {
        from: e.file.path().to_path_buf(),
        to: path.to_path_buf(),
        err: e.error,
    })?;
    Ok(())
}
