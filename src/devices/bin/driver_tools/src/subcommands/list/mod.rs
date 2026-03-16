// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use anyhow::{Context, Result};
use args::ListCommand;
use fidl_fuchsia_driver_development as fdd;
use futures::join;
use std::collections::HashSet;
use std::io::Write;

pub async fn list(
    cmd: ListCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    if cmd.verbose {
        writeln!(
            writer,
            "WARNING: The verbose flag is deprecated. Use `ffx driver show` instead."
        )?;
        return Ok(());
    }

    let empty: [String; 0] = [];
    let driver_info = fuchsia_driver_dev::get_driver_info(&driver_development_proxy, &empty);

    let driver_info = if cmd.loaded {
        // Query devices and create a hash set of loaded drivers.
        let device_info = fuchsia_driver_dev::get_device_info(
            &driver_development_proxy,
            &empty,
            /* exact_match= */ false,
        );

        // Await the futures concurrently.
        let (driver_info, device_info) = join!(driver_info, device_info);

        let loaded_driver_set: HashSet<String> = HashSet::from_iter(
            device_info?.into_iter().filter_map(|device_info| device_info.bound_driver_url),
        );

        // Filter the driver list by the hash set.
        driver_info?
            .into_iter()
            .filter(|driver| {
                let mut loaded = false;
                if let Some(ref url) = driver.url {
                    if loaded_driver_set.contains(url) {
                        loaded = true
                    }
                }
                loaded
            })
            .collect()
    } else {
        driver_info.await.context("Failed to get driver info")?
    };

    for driver in driver_info {
        if let Some(name) = driver.name {
            let url = driver.url.unwrap_or_default();
            writeln!(writer, "{:<20}: {}", name, url)?;
        } else {
            let url = driver.url.unwrap_or_default();
            writeln!(writer, "{}", url)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;
    use fidl::endpoints::ServerEnd;
    use fidl_fuchsia_driver_framework as fdf;
    use fuchsia_async as fasync;
    use futures::future::{Future, FutureExt};
    use futures::stream::StreamExt;

    /// Invokes `list` with `cmd` and runs a mock driver development server that
    /// invokes `on_driver_development_request` whenever it receives a request.
    /// The output of `list` that is normally written to its `writer` parameter
    /// is returned.
    async fn test_list<F, Fut>(cmd: ListCommand, on_driver_development_request: F) -> Result<String>
    where
        F: Fn(fdd::ManagerRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + Sync,
    {
        let (driver_development_proxy, mut driver_development_requests) =
            fidl::endpoints::create_proxy_and_stream::<fdd::ManagerMarker>();

        // Run the command and mock driver development server.
        let mut writer = Vec::new();
        let request_handler_task = fasync::Task::spawn(async move {
            while let Some(res) = driver_development_requests.next().await {
                let request = res.context("Failed to get next request")?;
                on_driver_development_request(request).await.context("Failed to handle request")?;
            }
            anyhow::bail!("Driver development request stream unexpectedly closed");
        });
        futures::select! {
            res = request_handler_task.fuse() => {
                res?;
                anyhow::bail!("Request handler task unexpectedly finished");
            }
            res = list(cmd, &mut writer, driver_development_proxy).fuse() => res.context("List command failed")?,
        }

        String::from_utf8(writer).context("Failed to convert list output to a string")
    }

    async fn run_driver_info_iterator_server(
        mut driver_infos: Vec<fdf::DriverInfo>,
        iterator: ServerEnd<fdd::DriverInfoIteratorMarker>,
    ) -> Result<()> {
        let mut iterator = iterator.into_stream();
        while let Some(res) = iterator.next().await {
            let request = res.context("Failed to get request")?;
            match request {
                fdd::DriverInfoIteratorRequest::GetNext { responder } => {
                    responder
                        .send(&driver_infos)
                        .context("Failed to send driver infos to responder")?;
                    driver_infos.clear();
                }
            }
        }
        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_list_all() {
        let cmd = ListCommand::from_args(&["list"], &[]).unwrap();

        let output = test_list(cmd, |request: fdd::ManagerRequest| async move {
            match request {
                fdd::ManagerRequest::GetDriverInfo {
                    driver_filter: _,
                    iterator,
                    control_handle: _,
                } => run_driver_info_iterator_server(
                    vec![fdf::DriverInfo {
                        name: Some("foo".to_owned()),
                        url: Some("fuchsia-pkg://fuchsia.com/foo-package#meta/foo.cm".to_owned()),
                        ..Default::default()
                    }],
                    iterator,
                )
                .await
                .context("Failed to run driver info iterator server")?,
                _ => {}
            }
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(
            output,
            "foo                 : fuchsia-pkg://fuchsia.com/foo-package#meta/foo.cm\n"
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_verbose_deprecated() {
        let cmd = ListCommand::from_args(&["list"], &["--verbose"]).unwrap();

        let output = test_list(cmd, |request: fdd::ManagerRequest| async move {
            match request {
                fdd::ManagerRequest::GetDriverInfo {
                    driver_filter: _,
                    iterator,
                    control_handle: _,
                } => run_driver_info_iterator_server(
                    vec![fdf::DriverInfo {
                        name: Some("foo".to_owned()),
                        url: Some("fuchsia-pkg://fuchsia.com/foo-package#meta/foo.cm".to_owned()),
                        ..Default::default()
                    }],
                    iterator,
                )
                .await
                .context("Failed to run driver info iterator server")?,
                _ => {}
            }
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(
            output,
            "WARNING: The verbose flag is deprecated. Use `ffx driver show` instead.\n"
        );
    }
}
