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
    example = "Press and release the 'a' key:
  $ ffx emu input key a

Send raw QMP input events:
  $ ffx emu input raw '[{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":7}}}}]'

Target a specific emulator and slow down the steps:
  $ ffx emu input --name fuchsia-emulator --interval 100 key a b c",
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

    /// time in milliseconds between consecutive key combos. Defaults to 20.
    #[argh(option, default = "20")]
    pub interval: u64,

    #[argh(subcommand)]
    pub subcommand: InputSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum InputSubCommand {
    Key(KeyCommand),
    Raw(RawCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "key",
    description = "Press and release keys.",
    example = "  $ ffx emu input key a
  $ ffx emu input key ctrl+alt+delete
  $ ffx emu input key shift+f10 ret
  $ ffx emu input key --down shift
  $ ffx emu input key --up shift",
    note = "Each combo is one or more QEMU QKeyCode names joined with '+'. Names are
case-sensitive and all lowercase, with no aliases: use `a`, not `A`, and `ret`
for the Enter/Return key. Common names:
  a..z  0..9  ret  esc  spc  tab  backspace  delete  insert
  shift  shift_r  ctrl  ctrl_r  alt  alt_r  meta_l  meta_r  menu
  up  down  left  right  home  end  pgup  pgdn  caps_lock
  f1..f24  kp_0..kp_9  kp_enter  minus  equal  comma  dot  slash
All names: https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#enum-QMP-ui.QKeyCode

The keys of a combo go down in the given order and come back up in reverse
order. --down and --up send only the presses or only the releases. They can't
be combined with positional combos or with each other."
)]
pub struct KeyCommand {
    /// key combos to press and release in turn, e.g. `a` or `ctrl+alt+delete`.
    #[argh(positional)]
    pub combos: Vec<String>,

    /// press the keys of this combo without releasing them.
    #[argh(option)]
    pub down: Option<String>,

    /// release the keys of this combo, in reverse order, without pressing them.
    #[argh(option)]
    pub up: Option<String>,

    /// time in milliseconds between the last press and the first release of a combo.
    /// Defaults to 20.
    #[argh(option, default = "20")]
    pub key_duration: u64,
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
        assert_eq!(cmd.interval, 20);

        let cmd = parse(&["--name", "emu1", "--interval", "50", "raw", "[]"]).unwrap();
        assert_eq!(cmd.name, Some("emu1".into()));
        assert_eq!(cmd.interval, 50);
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

    #[test]
    fn test_key_defaults() {
        let cmd = parse(&["key", "a"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            InputSubCommand::Key(KeyCommand {
                combos: vec!["a".into()],
                down: None,
                up: None,
                key_duration: 20
            })
        );
    }

    #[test]
    fn test_key_multiple_combos_and_duration() {
        let cmd = parse(&["key", "ctrl+alt+delete", "ret", "--key-duration", "100"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            InputSubCommand::Key(KeyCommand {
                combos: vec!["ctrl+alt+delete".into(), "ret".into()],
                down: None,
                up: None,
                key_duration: 100
            })
        );
    }

    #[test]
    fn test_key_down_up() {
        let cmd = parse(&["key", "--down", "shift"]).unwrap();
        let InputSubCommand::Key(key) = cmd.subcommand else { panic!() };
        assert_eq!(key.down, Some("shift".into()));
        assert!(key.combos.is_empty());

        let cmd = parse(&["key", "--up", "ctrl+alt"]).unwrap();
        let InputSubCommand::Key(key) = cmd.subcommand else { panic!() };
        assert_eq!(key.up, Some("ctrl+alt".into()));
    }
}
