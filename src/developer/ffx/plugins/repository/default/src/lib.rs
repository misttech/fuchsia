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

    type Error = ::fho::Error;

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
        SubCommand::Set(set) => {
            let env = context.load().map_err(ffx_config::macro_deps::anyhow::Error::from)?;
            let mut config = ffx_config::Config::from_env(&env)
                .map_err(ffx_config::macro_deps::anyhow::Error::from)?;
            config
                .set(CONFIG_KEY_DEFAULT, set.level, serde_json::Value::String(set.name.clone()))
                .map_err(ffx_config::macro_deps::anyhow::Error::from)?;
            config.save().map_err(ffx_config::macro_deps::anyhow::Error::from)?;
        }
        SubCommand::Unset(unset) => {
            let env = context.load().map_err(ffx_config::macro_deps::anyhow::Error::from)?;
            let mut config = ffx_config::Config::from_env(&env)
                .map_err(ffx_config::macro_deps::anyhow::Error::from)?;
            let res = (|| {
                config.remove(CONFIG_KEY_DEFAULT, unset.level)?;
                config.save()
            })();
            if let Err(e) = res {
                let _ = writeln!(writer.stderr(), "warning: {}", e);
            }
        }
    };
    Ok(())
}
