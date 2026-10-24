// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args::SerialCommand;
use discovery::gce_watcher;
use ffx_config::EnvironmentContext;
use ffx_gce::GceContext;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool, Result, return_user_error, user_error};
use std::io::Write;
use std::time::Duration;

const SERIAL_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, FfxTool)]
pub struct SerialTool {
    #[command]
    cmd: SerialCommand,
    context: EnvironmentContext,
}

impl FfxMain for SerialTool {
    type Writer = SimpleWriter;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        gce_watcher::Instance::validate_name(&self.cmd.name).map_err(|e| user_error!("{e}"))?;

        let gce = match GceContext::new(self.context, self.cmd.project, self.cmd.zone).await {
            Ok(c) => c,
            Err(e) => return_user_error!("{e}"),
        };
        let instance = gce_watcher::Instance::new(&gce.project, &gce.zone, &self.cmd.name)
            .map_err(|e| user_error!("{e}"))?;

        stream_serial_output(
            &gce,
            &instance,
            self.cmd.port,
            self.cmd.start,
            self.cmd.follow,
            &mut writer,
        )
        .await
    }
}

/// Reads or continuously streams serial port output from `instance` into `writer`.
pub async fn stream_serial_output<W: Write>(
    gce: &GceContext,
    instance: &gce_watcher::Instance,
    port: u32,
    start: Option<i64>,
    follow: bool,
    mut writer: W,
) -> Result<()> {
    let mut current_offset = start;

    loop {
        let had_new_data = match gce
            .client
            .get_serial_port_output(
                &instance.project,
                &instance.zone,
                &instance.name,
                port,
                current_offset,
            )
            .await
        {
            Ok(output) => {
                let has_data = !output.contents.is_empty();
                write_serial_output(&mut writer, &output.contents)?;
                current_offset = Some(output.next);
                has_data
            }
            Err(e) if e.is_forbidden() => {
                log::info!(
                    "REST getSerialPortOutput returned 403 Forbidden for '{}'; falling back to \
                     SSH serial console ({})",
                    instance.name,
                    gce.serial_endpoint()
                );
                return gce
                    .stream_ssh_serial(&instance.name, port, follow, &mut writer)
                    .await
                    .map_err(|ssh_err| user_error!("{ssh_err}"));
            }
            Err(e) => {
                return_user_error!("{e}");
            }
        };

        if !follow {
            break;
        }

        if !had_new_data {
            fuchsia_async::Timer::new(SERIAL_POLL_INTERVAL).await;
        }
    }

    Ok(())
}

fn write_serial_output<W: Write>(mut writer: W, contents: &str) -> Result<()> {
    if !contents.is_empty() {
        write!(writer, "{}", contents)?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_writer::TestBuffers;

    #[fuchsia::test]
    fn test_write_serial_output() {
        let test_buffers = TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        write_serial_output(&mut writer, "[00000.000] 00000:00000> Welcome to Zircon!\n").unwrap();
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(stderr.is_empty());
        assert_eq!(stdout, "[00000.000] 00000:00000> Welcome to Zircon!\n");
    }

    #[fuchsia::test]
    fn test_write_serial_output_empty() {
        let test_buffers = TestBuffers::default();
        let mut writer = SimpleWriter::new_test(&test_buffers);
        write_serial_output(&mut writer, "").unwrap();
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }
}
