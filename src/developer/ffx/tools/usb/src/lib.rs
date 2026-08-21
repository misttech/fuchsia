// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fho::subtool_suite::{FfxSubtoolSuite, SubtoolBox, SubtoolSuite, ToolSuiteCommand};
use fho::{FhoEnvironment, Result};

mod args;

pub use args::UsbCommand;

impl ToolSuiteCommand for UsbCommand {
    type SubCommand = ();
    fn into_subcommand(self) -> Self::SubCommand {
        ()
    }
}

pub struct UsbSuite;

#[async_trait::async_trait(?Send)]
impl SubtoolSuite for UsbSuite {
    type Command = UsbCommand;

    async fn new_subtool(_env: FhoEnvironment, _subcommand: ()) -> Result<Box<dyn SubtoolBox>> {
        Err(fho::Error::User(anyhow::anyhow!("No subcommands implemented yet")))
    }
}

pub type UsbSuiteTool = FfxSubtoolSuite<UsbSuite>;
