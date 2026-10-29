// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::args_info::CliArgsInfo;
use crate::{Error, FfxCommandLine, MetricsSession, Result};
use ffx_config::EnvironmentContext;
pub use ffx_futures::LocalBoxFuture;
use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::ExitStatus;

/// Where the command was discovered
#[derive(Clone, Copy, Debug, Ord, PartialOrd, PartialEq, Eq, Hash)]
pub enum FfxToolSource {
    /// built directly into the executable
    BuiltIn,
    /// discovered in a development tree or workspace tree
    Workspace,
    /// discovered in the currently active SDK
    Sdk,
}

/// Information about a tool for use in help output
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FfxToolInfo {
    /// Where the tool was found
    pub source: FfxToolSource,
    /// The short name of the tool
    pub name: String,
    /// A longer one-line description of the functionality of the tool
    pub description: String,
    /// A path to the executable for the tool, if there is one
    pub path: Option<PathBuf>,
}

impl FfxToolInfo {
    fn write_description(&self, out: &mut String) {
        crate::describe::write_description(out, &self.name, &self.description)
    }
}

impl From<&argh::CommandInfo> for FfxToolInfo {
    fn from(info: &argh::CommandInfo) -> Self {
        let source = FfxToolSource::BuiltIn;
        let name = info.name.to_owned();
        let description = info.description.to_owned();
        let path = None;
        FfxToolInfo { source, name, description, path }
    }
}

pub trait ToolRunner {
    fn run<'a>(
        self: Box<Self>,
        metrics: MetricsSession,
    ) -> LocalBoxFuture<'a, Result<ExitStatus, Error>>
    where
        Self: 'a;
}

/// Implements discovering and loading the subtools a particular ffx binary
/// is capable of running.
#[allow(async_fn_in_trait)]
pub trait ToolSuite: Sized {
    /// Initializes the tool suite from the ffx invocation's environment.
    fn from_env(env: &EnvironmentContext) -> Result<Self, Error>;

    /// Lists commands that should be available no matter how and where this tool
    /// is invoked.
    fn global_command_list() -> &'static [&'static argh::CommandInfo];

    /// Lists all commands reachable from the current context. Defaults to just
    /// the same set of commands as in [`Self::global_command_list`].
    async fn command_list(&self) -> Vec<FfxToolInfo> {
        Self::global_command_list().iter().copied().map(|cmd| cmd.into()).collect()
    }

    /// Parses the given command line information into a runnable command
    /// object.
    async fn try_from_args(
        &self,
        cmd: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>, Error>;

    /// Returns the tool runner for the given command. This works even if the
    /// subcommand args cannot be parsed successfully so the right tool runner can be found
    /// even if the required arguments are missing.
    /// This is intended to be used when
    /// attempting to generate the schema for the machine output of a command.
    async fn try_runner_from_name(
        &self,
        cmd: &FfxCommandLine,
    ) -> Result<Option<Box<dyn ToolRunner + '_>>, Error>;

    /// Parses the given command line information into a runnable command
    /// object, exiting and printing the early exit output if help is requested
    /// or an error occurs.
    async fn from_args(&self, cmd: &FfxCommandLine) -> Option<Box<dyn ToolRunner + '_>> {
        self.try_from_args(cmd).await.unwrap_or_else(|early_exit| {
            print!("{}", early_exit);

            std::process::exit(early_exit.exit_code())
        })
    }

    /// Prints out a list of the commands this suite has available
    async fn print_command_list(&self, w: &mut impl Write) -> Result<(), std::fmt::Error> {
        print_command_list(w, &self.command_list().await, None)
    }

    /// Prints out a list of all commands and their subcommands this suite has available
    async fn print_all_commands(&self, w: &mut impl Write) -> Result<(), std::fmt::Error> {
        let args_info = self.get_args_info().await.unwrap_or_default();
        print_command_list(w, &self.command_list().await, Some(&args_info))
    }

    /// Finds the given tool by name in the available command list
    async fn find_tool_by_name(&self, name: &str) -> Option<FfxToolInfo> {
        self.command_list().await.into_iter().find(|cmd| cmd.name == name)
    }

    /// Gets the command line argument information.
    async fn get_args_info(&self) -> Result<CliArgsInfo, Error>;

    /// Extracts the subcommand chain for analytics from the suite's static argument info.
    fn subcommand_chain(&self, _cmd: &FfxCommandLine) -> Vec<String> {
        Vec::new()
    }
}

fn print_command_list(
    w: &mut impl Write,
    commands: &[FfxToolInfo],
    args_info: Option<&CliArgsInfo>,
) -> Result<(), std::fmt::Error> {
    let mut found = HashSet::new();
    let mut built_in = None;
    let mut workspace = None;
    let mut sdk = None;
    let mut sorted_commands = commands.to_vec();
    sorted_commands.sort_by(|a, b| match a.name.cmp(&b.name) {
        std::cmp::Ordering::Less => std::cmp::Ordering::Less,
        std::cmp::Ordering::Greater => std::cmp::Ordering::Greater,
        std::cmp::Ordering::Equal => a.source.cmp(&b.source),
    });
    for cmd in sorted_commands.into_iter() {
        use FfxToolSource::*;
        if !found.contains(&cmd.name) {
            found.insert(cmd.name.clone());
            let kind = match cmd.source {
                BuiltIn => built_in.get_or_insert_with(String::new),
                Workspace => workspace.get_or_insert_with(String::new),
                Sdk => sdk.get_or_insert_with(String::new),
            };
            cmd.write_description(kind);
            if let Some(sub_info) =
                args_info.and_then(|info| info.commands.iter().find(|s| s.name == cmd.name))
            {
                write_subcommands_recursive(kind, &sub_info.command.commands, 1);
            }
        }
    }

    if let Some(built_in) = built_in {
        writeln!(w, "Built-in Commands:\n{built_in}\n")?;
    }
    if let Some(workspace) = workspace {
        writeln!(w, "Workspace Commands:\n{workspace}\n")?;
    }
    if let Some(sdk) = sdk {
        writeln!(w, "SDK Commands:\n{sdk}\n")?;
    }
    Ok(())
}

fn write_subcommands_recursive(
    out: &mut String,
    subcommands: &[crate::args_info::SubCommandInfo],
    depth: usize,
) {
    let mut seen = HashSet::<&str>::new();
    let mut sorted_subcommands: Vec<_> = subcommands.iter().collect();
    sorted_subcommands.sort_by(|a, b| a.name.cmp(&b.name));
    for subcommand in sorted_subcommands {
        if seen.insert(&subcommand.name) {
            crate::describe::write_description_with_indent(
                out,
                &subcommand.name,
                &subcommand.command.description,
                depth,
            );
            if !subcommand.command.commands.is_empty() {
                write_subcommands_recursive(out, &subcommand.command.commands, depth + 1);
            }
        }
    }
}
