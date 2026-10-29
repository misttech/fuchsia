// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "input",
    description = "Send input events to a running emulator.",
    example = "Send raw QMP input events:
  $ ffx emu input raw '[{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":7}}}}]'",
    note = "Events are injected into the emulator's virtual input devices through the
QEMU Machine Protocol (QMP), so they go through the guest's input drivers.
Use `print-input-report` on the target to see the resulting reports.
Only the qemu engine is supported.

Each step, such as one key press or one key release, becomes one input report
in the guest. The exception is `raw` key events given by scancode number:
QEMU sends each of them as its own report."
)]
pub struct InputCommand {
    /// name of the emulator instance to send input to, as listed by `ffx emu list`.
    /// Defaults to the only running instance.
    #[argh(option)]
    pub name: Option<String>,

    #[argh(subcommand)]
    pub subcommand: InputSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum InputSubCommand {
    Raw(RawCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "raw",
    description = "Send raw QMP input events, unchanged, in one QMP command.",
    example = "  $ ffx emu input raw '[{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":7}}}}]'",
    note = "The argument is a JSON array of QMP InputEvent objects. It is sent as the
`events` of one `input-send-event` command. See
https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#command-QMP-ui.input-send-event

The guest receives the events as one input report, except key events given by
scancode number: QEMU sends each of them as its own report."
)]
pub struct RawCommand {
    /// a JSON array of QMP InputEvent objects.
    #[argh(positional)]
    pub json: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<InputCommand, argh::EarlyExit> {
        InputCommand::from_args(&["input"], args)
    }

    #[test]
    fn test_global_options() {
        let cmd = parse(&["raw", "[]"]).unwrap();
        assert_eq!(cmd.name, None);

        let cmd = parse(&["--name", "emu1", "raw", "[]"]).unwrap();
        assert_eq!(cmd.name, Some("emu1".into()));
    }

    #[test]
    fn test_no_short_name_flag() {
        assert!(parse(&["-n", "emu1", "raw", "[]"]).is_err());
    }

    #[test]
    fn test_missing_subcommand() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn test_raw() {
        let json = r#"[{"type":"rel","data":{"axis":"x","value":7}}]"#;
        let cmd = parse(&["raw", json]).unwrap();
        assert_eq!(cmd.subcommand, InputSubCommand::Raw(RawCommand { json: json.into() }));
        assert!(parse(&["raw"]).is_err());
    }
}
