// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::UtilsError;
use fidl::endpoints::{ClientEnd, create_endpoints, create_proxy};
use fidl_fuchsia_io as fio;

pub async fn open_pkg_file(
    pkg_dir: &fio::DirectoryProxy,
    relative_binary_path: &str,
) -> Result<zx::Vmo, UtilsError> {
    let (file, server) = create_proxy::<fio::FileMarker>();
    pkg_dir.open(
        relative_binary_path,
        fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        &fio::Options::default(),
        server.into_channel(),
    )?;

    let vmo = file
        .get_backing_memory(
            fio::VmoFlags::READ | fio::VmoFlags::EXECUTE | fio::VmoFlags::PRIVATE_CLONE,
        )
        .await??;
    Ok(vmo)
}

pub fn open_lib_dir(
    pkg_dir: &fio::DirectoryProxy,
) -> Result<ClientEnd<fio::DirectoryMarker>, UtilsError> {
    let (lib_dir, server) = create_endpoints::<fio::DirectoryMarker>();
    pkg_dir.open(
        "lib",
        fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        &fio::Options::default(),
        server.into_channel(),
    )?;
    Ok(lib_dir)
}
