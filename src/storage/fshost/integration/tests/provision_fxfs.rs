// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod config;
use crate::config::new_builder;
use fshost_test_fixture::VFS_TYPE_FXFS;
use fshost_test_fixture::disk_builder::{Disk, DiskBuilder};

#[cfg(feature = "fxblob")]
#[fuchsia::test]
async fn test_provision_fxfs() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("provision_fxfs", true);
    builder.fshost().set_config_value("merge_super_and_userdata", true);
    let mut fixture = builder.build().await;

    let mut disk = DiskBuilder::new();
    // Use unformatted volume manager to build an unformatted disk
    disk.with_gpt()
        .with_unformatted_volume_manager()
        .with_system_partition_label("super")
        .with_extra_gpt_partition("userdata", 1)
        .with_extra_gpt_partition("other", 1);
    fixture.add_main_disk(Disk::Builder(disk)).await;

    // TODO(https://fxbug.dev/439942311): Don't emit error messages for non-errors.
    // We expect one crash report when first attempting to mount and serve fxblob before fxfs has
    // been provisioned.
    fixture.wait_for_crash_reports(1, "fxfs", "fuchsia-fxfs-corruption").await;

    fixture.check_system_partitions(vec!["other", "super_and_userdata"]).await;
    fixture.check_fs_type("data", VFS_TYPE_FXFS).await;
    fixture.check_test_data_file().await;

    fixture.tear_down().await;
}
