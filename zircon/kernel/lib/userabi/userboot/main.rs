// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow};
use bootfs::get_bootfs_vmo;
use cmdline::{ProgramInfo, parse_cmdline};
use fidl_next::fuchsia::create_channel;
use fidl_next_fuchsia_boot as fuchsia_boot;
use fidl_next_fuchsia_io as fuchsia_io;
use fuchsia_runtime::{HandleInfo, HandleType};
use mapped_vmo::ImmutableMapping;
use process_builder::StartupHandle;
use program::{SystemHandles, launch_program};
use std::fmt::Write as _;
use userboot::{Log, take_system_handles};
use zbi::{ZbiContainer, ZbiType};
use zerocopy::IntoBytes as _;

const DEFAULT_NEXT_BOOT: &str = "bin/component_manager+--boot";

/// Program configuration options parsed from the command line.
#[derive(Default)]
struct Options {
    /// Information about the primary boot program to launch.
    boot: ProgramInfo,
    /// Whether to check the CRC of bootfs files.
    bootfs_crc_check: bool,
}

fn parse_option(key: &str, value: &str, opts: &mut Options) -> bool {
    match key {
        "userboot.next" => {
            opts.boot.next = value.to_string();
            true
        }
        "userboot.root" => {
            opts.boot.root = value.strip_suffix('/').unwrap_or(value).to_string();
            true
        }
        "userboot.crc" => {
            opts.bootfs_crc_check = value == "true";
            true
        }
        _ => false,
    }
}

fn get_options(container: &ZbiContainer<&[u8]>, log: &mut Log) -> Result<Options, Error> {
    let mut options = Options::default();
    for item in container.iter() {
        if item.header.type_ == ZbiType::CmdLine as u32 {
            if let Ok(cmdline) = str::from_utf8(item.payload.as_bytes()) {
                writeln!(log, "CMDLINE {}", cmdline)?;
                parse_cmdline(cmdline, log, &mut options, parse_option)?;
            }
        }
    }

    if options.boot.next.is_empty() {
        options.boot.next = DEFAULT_NEXT_BOOT.to_string();
    }

    if !options.boot.root.is_empty() && options.boot.root.starts_with('/') {
        anyhow::bail!("`userboot.root` (\"{}\") must not begin with a '/'", options.boot.root);
    }

    Ok(options)
}

async fn run(log: &mut Log) -> Result<(), Error> {
    let mut handles = SystemHandles::from_handles(take_system_handles()?)?;

    let (options, bootfs_vmo) = {
        let zbi = ImmutableMapping::create_from_vmo(&handles.zbi_vmo, true)?;
        let container =
            ZbiContainer::parse(&zbi[..]).map_err(|e| anyhow!("Failed to parse ZBI: {e:?}"))?;
        let options = get_options(&container, log)?;
        let bootfs_vmo = get_bootfs_vmo(&container, options.bootfs_crc_check, log)?;
        (options, bootfs_vmo)
    };

    let (program_name, target_path) = options.boot.filename();

    let (userboot_client, userboot_server) = create_channel::<fuchsia_boot::Userboot>();
    let userboot_client = userboot_client.spawn();
    let (svc_stash_client, svc_stash_server) = create_channel::<fuchsia_boot::SvcStash>();
    let svc_stash_client = svc_stash_client.spawn();
    let _ = userboot_client.post_stash_svc(svc_stash_server).await;

    let (svc_client, svc_server) = create_channel::<fuchsia_io::Directory>();
    let _ = svc_stash_client.store(svc_server).await;

    handles.startup_handles.push(StartupHandle {
        handle: svc_client.into_untyped().into_handle().into(),
        info: HandleInfo::new(HandleType::NamespaceDirectory, 0),
    });
    handles.startup_handles.push(StartupHandle {
        handle: userboot_server.into_untyped().into_handle().into(),
        info: HandleInfo::new(HandleType::User0, 0),
    });

    let mut bootfs_entries = Vec::new();
    let _process = launch_program(
        program_name,
        &target_path,
        options.boot.next.split('+'),
        bootfs_vmo,
        handles,
        Some(&mut bootfs_entries),
        log,
    )
    .await
    .map_err(|e| anyhow!("launch_program failed: {e:?}"))?;

    if !bootfs_entries.is_empty() {
        let files = bootfs_entries
            .into_iter()
            .map(|(offset, contents)| fuchsia_boot::natural::BootfsFileVmo { offset, contents })
            .collect::<Vec<_>>();
        let _ = userboot_client.post_bootfs_files(files).await;
    }

    Ok(())
}

#[fuchsia_async::run_singlethreaded]
async fn main() {
    let mut log = Log::new();
    if let Err(e) = run(&mut log).await {
        let _ = writeln!(log, "userboot error: {e:?}");
    }
}
