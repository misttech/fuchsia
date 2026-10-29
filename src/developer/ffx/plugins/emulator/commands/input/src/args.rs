// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;
use std::num::NonZero;
use std::str::FromStr;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "input",
    description = "Send input events to a running emulator.",
    example = "Press and release the 'a' key:
  $ ffx emu input key a

Type text:
  $ ffx emu input text 'Hello, world!'

Move the mouse and click:
  $ ffx emu input mouse move --dx 10 --dy -5
  $ ffx emu input mouse click left

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

    /// time in milliseconds between consecutive key combos, characters and scroll ticks.
    /// Defaults to 20.
    #[argh(option, default = "20")]
    pub interval: u64,

    #[argh(subcommand)]
    pub subcommand: InputSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum InputSubCommand {
    Key(KeyCommand),
    Text(TextCommand),
    Mouse(MouseCommand),
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
    name = "text",
    description = "Type printable ASCII text using a US QWERTY layout.",
    example = "  $ ffx emu input text 'Hello, world!'",
    note = "Only printable ASCII (0x20-0x7E) is accepted. Shift is added where needed.
Use `ffx emu input key` for keys such as ret or tab."
)]
pub struct TextCommand {
    /// the text to type.
    #[argh(positional)]
    pub text: String,

    /// time in milliseconds each key is held down. Defaults to 20.
    #[argh(option, default = "20")]
    pub key_duration: u64,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "mouse",
    description = "Send relative mouse events.",
    note = "The mouse commands need the virtual device's pointing device to be 'mouse'.
Check it with `ffx emu show`."
)]
pub struct MouseCommand {
    #[argh(subcommand)]
    pub subcommand: MouseSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(subcommand)]
pub enum MouseSubCommand {
    Move(MouseMoveCommand),
    Click(MouseClickCommand),
    Down(MouseDownCommand),
    Up(MouseUpCommand),
    Scroll(MouseScrollCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "move",
    description = "Move the mouse by a relative amount, in one report.",
    example = "  $ ffx emu input mouse move --dx 10 --dy -5"
)]
pub struct MouseMoveCommand {
    /// horizontal motion. Positive moves right. Defaults to 0.
    #[argh(option, default = "0")]
    pub dx: i64,

    /// vertical motion. Positive moves down. Defaults to 0.
    #[argh(option, default = "0")]
    pub dy: i64,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "click",
    description = "Press and release a mouse button.",
    example = "  $ ffx emu input mouse click left
  $ ffx emu input mouse click right --button-duration 50"
)]
pub struct MouseClickCommand {
    /// the button: left, right, middle, side or extra.
    #[argh(positional)]
    pub button: MouseButton,

    /// time in milliseconds the button is held down. Defaults to 20.
    #[argh(option, default = "20")]
    pub button_duration: u64,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "down",
    description = "Press a mouse button without releasing it.",
    example = "  $ ffx emu input mouse down left"
)]
pub struct MouseDownCommand {
    /// the button: left, right, middle, side or extra.
    #[argh(positional)]
    pub button: MouseButton,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "up",
    description = "Release a mouse button.",
    example = "  $ ffx emu input mouse up left"
)]
pub struct MouseUpCommand {
    /// the button: left, right, middle, side or extra.
    #[argh(positional)]
    pub button: MouseButton,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq, Clone)]
#[argh(
    subcommand,
    name = "scroll",
    description = "Scroll the mouse wheel along one axis.",
    example = "  $ ffx emu input mouse scroll --dy 3
  $ ffx emu input mouse scroll --dx -1",
    note = "Specify exactly one of --dy and --dx. Each tick is one report."
)]
pub struct MouseScrollCommand {
    /// vertical wheel ticks. Positive scrolls up.
    #[argh(option)]
    pub dy: Option<NonZero<i64>>,

    /// horizontal wheel ticks. Positive scrolls right.
    #[argh(option)]
    pub dx: Option<NonZero<i64>>,
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

/// A mouse button that can be named on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Side,
    Extra,
}

impl FromStr for MouseButton {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(MouseButton::Left),
            "right" => Ok(MouseButton::Right),
            "middle" => Ok(MouseButton::Middle),
            "side" => Ok(MouseButton::Side),
            "extra" => Ok(MouseButton::Extra),
            _ => Err(format!(
                "unknown mouse button '{s}'. Expected one of: left, right, middle, side, extra"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<InputCommand, argh::EarlyExit> {
        InputCommand::from_args(&["input"], args)
    }

    fn parse_mouse(args: &[&str]) -> MouseSubCommand {
        let mut all_args = vec!["mouse"];
        all_args.extend_from_slice(args);
        let InputSubCommand::Mouse(mouse) = parse(&all_args).unwrap().subcommand else {
            panic!("not a mouse command");
        };
        mouse.subcommand
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

    #[test]
    fn test_text() {
        let cmd = parse(&["text", "hi"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            InputSubCommand::Text(TextCommand { text: "hi".into(), key_duration: 20 })
        );
        let cmd = parse(&["text", "hi", "--key-duration", "5"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            InputSubCommand::Text(TextCommand { text: "hi".into(), key_duration: 5 })
        );
        assert!(parse(&["text"]).is_err());
    }

    #[test]
    fn test_mouse_move_negative() {
        assert_eq!(
            parse_mouse(&["move", "--dx", "-10", "--dy", "5"]),
            MouseSubCommand::Move(MouseMoveCommand { dx: -10, dy: 5 })
        );
        assert_eq!(
            parse_mouse(&["move", "--dy", "-7"]),
            MouseSubCommand::Move(MouseMoveCommand { dx: 0, dy: -7 })
        );
    }

    #[test]
    fn test_mouse_click() {
        assert_eq!(
            parse_mouse(&["click", "right", "--button-duration", "5"]),
            MouseSubCommand::Click(MouseClickCommand {
                button: MouseButton::Right,
                button_duration: 5
            })
        );
        assert_eq!(
            parse_mouse(&["click", "middle"]),
            MouseSubCommand::Click(MouseClickCommand {
                button: MouseButton::Middle,
                button_duration: 20
            })
        );
        assert!(parse(&["mouse", "click", "left", "--count", "2"]).is_err());
    }

    #[test]
    fn test_mouse_down_up() {
        assert_eq!(
            parse_mouse(&["down", "side"]),
            MouseSubCommand::Down(MouseDownCommand { button: MouseButton::Side })
        );
        assert_eq!(
            parse_mouse(&["up", "extra"]),
            MouseSubCommand::Up(MouseUpCommand { button: MouseButton::Extra })
        );
    }

    #[test]
    fn test_mouse_scroll() {
        assert_eq!(
            parse_mouse(&["scroll", "--dy", "-3"]),
            MouseSubCommand::Scroll(MouseScrollCommand { dy: NonZero::new(-3), dx: None })
        );
        assert_eq!(
            parse_mouse(&["scroll", "--dx", "2"]),
            MouseSubCommand::Scroll(MouseScrollCommand { dy: None, dx: NonZero::new(2) })
        );
        assert!(parse(&["mouse", "scroll", "--dy", "0"]).is_err());
    }

    #[test]
    fn test_mouse_buttons() {
        for (name, button) in [
            ("left", MouseButton::Left),
            ("right", MouseButton::Right),
            ("middle", MouseButton::Middle),
            ("side", MouseButton::Side),
            ("extra", MouseButton::Extra),
        ] {
            assert_eq!(name.parse::<MouseButton>(), Ok(button));
        }
        assert!(parse(&["mouse", "click", "wheel-up"]).is_err());
        assert!(parse(&["mouse", "click", "LEFT"]).is_err());
        assert!(parse(&["mouse"]).is_err());
    }
}
