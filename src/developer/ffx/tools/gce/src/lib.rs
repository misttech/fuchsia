// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fho::subtool_suite::{FfxSubtoolSuite, Subtool, SubtoolBox, SubtoolSuite, ToolSuiteCommand};
use fho::{FfxTool, FhoEnvironment, Result};

mod args;
mod subtools;

pub use args::{GceCommand, GceSubCommand, ListCommand, ShowCommand};
pub use subtools::{ListTool, ShowTool};

impl ToolSuiteCommand for GceCommand {
    type SubCommand = GceSubCommand;
    fn into_subcommand(self) -> Self::SubCommand {
        self.subcommand
    }
}

pub struct GceSuite;

#[async_trait::async_trait(?Send)]
impl SubtoolSuite for GceSuite {
    type Command = GceCommand;

    async fn new_subtool(
        env: FhoEnvironment,
        subcommand: GceSubCommand,
    ) -> Result<Box<dyn SubtoolBox>> {
        Ok(match subcommand {
            GceSubCommand::List(cmd) => Subtool::new(ListTool::from_env(env, cmd).await?),
            GceSubCommand::Show(cmd) => Subtool::new(ShowTool::from_env(env, cmd).await?),
        })
    }
}

pub type GceSuiteTool = FfxSubtoolSuite<GceSuite>;

#[cfg(test)]
mod tests {
    use super::*;
    use argh::FromArgs;

    #[test]
    fn test_parse_list_command() {
        let cmd = GceCommand::from_args(&["gce"], &["list", "--zone", "us-east1-c"])
            .expect("parsed list");
        match cmd.subcommand {
            GceSubCommand::List(l) => {
                assert_eq!(l.zone.as_deref(), Some("us-east1-c"));
            }
            _ => panic!("expected List subcommand"),
        }
    }

    #[test]
    fn test_parse_show_command() {
        let cmd = GceCommand::from_args(&["gce"], &["show", "test-vm"]).expect("parsed show");
        match cmd.subcommand {
            GceSubCommand::Show(s) => {
                assert_eq!(s.name, "test-vm");
            }
            _ => panic!("expected Show subcommand"),
        }
    }
}
