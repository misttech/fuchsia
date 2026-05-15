// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[derive(Debug, thiserror::Error)]
pub enum DoctorUtilsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Daemon core error: {0}")]
    Daemon(#[source] Box<ffx_daemon::DaemonError>),

    #[error("Pgrep/Pkill execution status code: {0}")]
    ProcessStatusCode(i32),

    #[error("Pgrep/Pkill execution error")]
    ProcessStatusError,

    #[error("Zip archive error: {0}")]
    Zip(#[from] zip::result::ZipError),
}

impl From<ffx_daemon::DaemonError> for DoctorUtilsError {
    fn from(err: ffx_daemon::DaemonError) -> Self {
        Self::Daemon(Box::new(err))
    }
}
