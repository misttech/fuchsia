// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use async_trait::async_trait;
use fdomain_fuchsia_element::ManagerProxy;
use ffx_session_remove_args::SessionRemoveCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use target_holders::fdomain::moniker;

#[derive(FfxTool)]
pub struct RemoveTool {
    #[command]
    cmd: SessionRemoveCommand,
    #[with(moniker("/core/session-manager"))]
    manager_proxy: ManagerProxy,
}

fho::embedded_plugin!(RemoveTool);

#[async_trait(?Send)]
impl FfxMain for RemoveTool {
    // TODO(b/472310565) Support actual "json" output, not just "raw"
    type Writer = SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        remove_impl(self.manager_proxy, self.cmd, &mut writer).await?;
        Ok(())
    }
}

pub async fn remove_impl<W: std::io::Write>(
    manager_proxy: ManagerProxy,
    cmd: SessionRemoveCommand,
    writer: &mut W,
) -> Result<()> {
    writeln!(writer, "Remove {} from the current session", &cmd.name)?;

    manager_proxy.remove_element(&cmd.name).await?.map_err(|err| format_err!("{:?}", err))?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use fdomain_fuchsia_element::ManagerRequest;
    use target_holders::fdomain::fake_proxy;

    #[fuchsia::test]
    async fn test_remove_element() {
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, |req| match req {
            ManagerRequest::ProposeElement { .. } => unreachable!(),
            ManagerRequest::RemoveElement { name, responder } => {
                assert_eq!(name, "foo");
                let _ = responder.send(Ok(()));
            }
        });

        let remove_cmd = SessionRemoveCommand { name: "foo".to_string() };
        let mut writer = Vec::new();
        let response = remove_impl(proxy, remove_cmd, &mut writer).await;
        assert!(response.is_ok());
    }
}
