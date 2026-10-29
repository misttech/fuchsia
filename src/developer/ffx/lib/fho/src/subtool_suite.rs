// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{FfxMain, FfxTool, FhoEnvironment, TryFromEnv};
use argh::{ArgsInfo, FromArgs, SubCommand};
use ffx_command::{Error, LocalBoxFuture, Result};
use std::cell::RefCell;

// We need this wrapper because we want to store the subtool as a dyn
// FfxTool, but FfxTool has associated types, so it doesn't really work the
// way we want it to. For instance, you can't return a Box<dyn FfxTool>,
// since the associated types have to be specified. Instead, we store a
// Subtool<F: FfxTool>, which implements this trait.
pub trait SubtoolBox {
    fn supports_structured_output(&self) -> bool;
    fn has_schema(&self) -> bool;
    fn try_print_schema<'a>(&'a self, env: &'a FhoEnvironment) -> LocalBoxFuture<'a, Result<()>>;
    fn run<'a>(&'a self, env: FhoEnvironment) -> LocalBoxFuture<'a, Result<()>>;
}

/// A container for an `FfxTool` -- wrap the subcommand FfxTool in this
/// object in your implementation of SubtoolSuite::new_subtool().
pub struct Subtool<T: FfxTool> {
    // We store the tool in a RefCell<Option<T>> because some methods of FfxTool
    // take ownership of the tool, so we have to be ready to hand it over.
    tool: RefCell<Option<T>>,
}

impl<T: FfxTool> Subtool<T> {
    pub fn new(tool: T) -> Box<Self> {
        Box::new(Self { tool: RefCell::new(Some(tool)) })
    }
}

impl<T: FfxTool> SubtoolBox for Subtool<T> {
    fn supports_structured_output(&self) -> bool {
        self.tool.borrow().as_ref().expect("subtool is gone?").supports_structured_output()
    }
    fn has_schema(&self) -> bool {
        self.tool.borrow().as_ref().expect("subtool is gone?").has_schema()
    }
    fn try_print_schema<'a>(&'a self, env: &'a FhoEnvironment) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let writer = <<T as FfxMain>::Writer as TryFromEnv>::try_from_env(env)
                .await
                .map_err(|e| ffx_command_error::Error::Unexpected(anyhow::Error::new(e)))?;
            self.tool.borrow_mut().take().expect("subtool is gone?").try_print_schema(writer).await
        })
    }

    fn run<'a>(&'a self, env: FhoEnvironment) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let writer = <<T as FfxMain>::Writer as TryFromEnv>::try_from_env(&env)
                .await
                .map_err(|e| ffx_command_error::Error::Unexpected(anyhow::Error::new(e)))?;
            self.tool
                .borrow_mut()
                .take()
                .expect("subtool is gone?")
                .main(writer)
                .await
                .map_err(|e| e.into())
        })
    }
}

/// A trait for extracting a subcommand from a command. The implementation is likely simply:
/// ```
/// impl ToolSuiteCommand for MyCommand {
///     type SubCommand = MySubCommand;
///     fn into_subcommand(self) -> Self::SubCommand {
///         self.subcommand
///     }
/// }
/// ```
pub trait ToolSuiteCommand {
    type SubCommand;
    fn into_subcommand(self) -> Self::SubCommand;
}

/// A trait for implementing a suite of subtools. The implementation is likely simply:
/// ```
/// impl SubtoolSuite for MySuite {
///     type Command = MyCommand;
///
///     async fn new_subtool(
///         env: FhoEnvironment,
///         subcommand: MySubCommand,
///     ) -> Result<Box<dyn SubtoolBox>> {
///         Ok(match subcommand {
///             MySubCommand::Subtool1(cmd) => {
///                 Subtool::new(ffx_my_subtool1::MySubtool1::from_env(env, cmd).await?)
///             }
///             MySubCommand::Subtool2(cmd) => {
///                 Subtool::new(ffx_my_subtool2::MySubtool2::from_env(env, cmd).await?)
///             }
///             ...
///         })
///     }
/// }
/// ```
#[allow(async_fn_in_trait)]
pub trait SubtoolSuite {
    type Command: ToolSuiteCommand + FromArgs + SubCommand + ArgsInfo;
    async fn new_subtool(
        env: FhoEnvironment,
        subcommand: <Self::Command as ToolSuiteCommand>::SubCommand,
    ) -> Result<Box<dyn SubtoolBox>>;
}

/// The wrapper for the suite. Usually used as:
/// ```
/// pub type MySuiteTool = FfxSubtoolSuite<MySuite>;
/// ...
/// #[fuchsia_async::run_singlethreaded]
/// async fn main() {
///     MySuiteTool::execute_tool().await
/// }
/// ```
pub struct FfxSubtoolSuite<S: SubtoolSuite> {
    env: FhoEnvironment,
    subtool: Box<dyn SubtoolBox>,
    _marker: std::marker::PhantomData<S>,
}

impl<S: SubtoolSuite> FfxTool for FfxSubtoolSuite<S> {
    type Command = S::Command;
    async fn from_env(env: FhoEnvironment, cmd: Self::Command) -> Result<Self> {
        let subcommand = cmd.into_subcommand();
        let subtool = Box::pin(S::new_subtool(env.clone(), subcommand)).await?;
        Ok(Self { subtool, env, _marker: std::marker::PhantomData })
    }

    fn supports_structured_output(&self) -> bool {
        self.subtool.supports_structured_output()
    }
    fn has_schema(&self) -> bool {
        self.subtool.has_schema()
    }
    fn requires_target() -> bool {
        false
    }
}

impl<S: SubtoolSuite> FfxMain for FfxSubtoolSuite<S> {
    type Writer = crate::null_writer::NullWriter;
    type Error = Error;
    async fn main(self, _writer: Self::Writer) -> std::result::Result<(), Self::Error> {
        self.subtool.run(self.env).await
    }
    async fn try_print_schema(self, _writer: Self::Writer) -> Result<()> {
        self.subtool.try_print_schema(&self.env).await
    }
}
