// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_config::EnvironmentContext;
use ffx_repository_default_args::{RepositoryDefaultCommand, SubCommand};
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool, Result, bug};

pub(crate) const CONFIG_KEY_DEFAULT: &str = "repository.default";

#[derive(FfxTool)]
pub struct RepoDefaultTool {
    #[command]
    pub cmd: RepositoryDefaultCommand,
    context: EnvironmentContext,
}

fho::embedded_plugin!(RepoDefaultTool);

#[async_trait::async_trait(?Send)]
impl FfxMain for RepoDefaultTool {
    type Writer = SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        exec_repository_default_impl(&self.context, self.cmd, &mut writer).await
    }
}

pub async fn exec_repository_default_impl<W: std::io::Write + ToolIO>(
    context: &EnvironmentContext,
    cmd: RepositoryDefaultCommand,
    writer: &mut W,
) -> Result<()> {
    match &cmd.subcommand {
        SubCommand::Get(_) => {
            let res: String = context.get(CONFIG_KEY_DEFAULT).unwrap_or_else(|_| "".to_owned());
            writeln!(writer, "{}", res).map_err(|e| bug!(e))?;
        }
        SubCommand::Set(set) => context
            .query(CONFIG_KEY_DEFAULT)
            .level(Some(set.level))
            .build()
            .set(context, serde_json::Value::String(set.name.clone()))?,
        SubCommand::Unset(unset) => {
            let _ = context
                .query(CONFIG_KEY_DEFAULT)
                .level(Some(unset.level))
                .build()
                .remove(context)
                .map_err(|e| writeln!(writer.stderr(), "warning: {}", e));
        }
    };
    Ok(())
}
