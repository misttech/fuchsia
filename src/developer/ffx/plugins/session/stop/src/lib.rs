// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use async_trait::async_trait;
use fdomain_fuchsia_session::LifecycleProxy;
use ffx_session_stop_args::SessionStopCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use target_holders::fdomain::moniker;

const STOPPING_SESSION: &str = "Stopping the session\n";

#[derive(FfxTool)]
pub struct StopTool {
    #[command]
    cmd: SessionStopCommand,
    #[with(moniker("/core/session-manager"))]
    lifecycle_proxy: LifecycleProxy,
}

fho::embedded_plugin!(StopTool);

#[async_trait(?Send)]
impl FfxMain for StopTool {
    // TODO(b/472310565) Support actual "json" output, not just "raw"
    type Writer = SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        stop_impl(self.lifecycle_proxy, self.cmd, &mut writer).await?;
        Ok(())
    }
}

pub async fn stop_impl<W: std::io::Write>(
    lifecycle_proxy: LifecycleProxy,
    _cmd: SessionStopCommand,
    writer: &mut W,
) -> Result<()> {
    write!(writer, "{}", STOPPING_SESSION)?;
    lifecycle_proxy.stop().await?.map_err(|err| format_err!("{:?}", err))
}

#[cfg(test)]
mod test {
    use super::*;
    use fdomain_fuchsia_session::LifecycleRequest;
    use target_holders::fdomain::fake_proxy;

    #[fuchsia::test]
    async fn test_stop_session() -> Result<()> {
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, |req| match req {
            LifecycleRequest::Stop { responder } => {
                let _ = responder.send(Ok(()));
            }
            _ => panic!("Unxpected Lifecycle request"),
        });

        let stop_cmd = SessionStopCommand {};
        let mut writer = Vec::new();
        stop_impl(proxy, stop_cmd, &mut writer).await?;
        let output = String::from_utf8(writer).unwrap();
        assert_eq!(output, STOPPING_SESSION);
        Ok(())
    }
}
