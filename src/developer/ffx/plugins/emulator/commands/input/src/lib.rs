// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `ffx emu input`: injects input events into a running emulator.

use emulator_instance::EmulatorInstances;
use ffx_config::EnvironmentContext;
use ffx_config::keys::EMU_INSTANCE_ROOT_DIR;
use ffx_emulator_config::{EmulatorEngine, InputEvent, InputStep, PressState};
use ffx_emulator_engines::EngineBuilder;
use ffx_emulator_input_args::{InputCommand, InputSubCommand, KeyCommand, RawCommand, TextCommand};
use ffx_writer::VerifiedMachineWriter;
use fho::{FfxMain, FfxTool, Result as FhoResult, bug, return_user_error, user_error};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

mod keymap;

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct InputResult {
    /// The name of the emulator instance the input was sent to.
    pub instance: String,
    /// The number of steps (QMP `input-send-event` commands) sent.
    pub steps_sent: usize,
    /// The total number of input events sent.
    pub events_sent: usize,
}

#[derive(FfxTool)]
#[target(None)]
pub struct InputTool {
    #[command]
    pub cmd: InputCommand,
    pub context: EnvironmentContext,
}

fho::embedded_plugin!(InputTool);

impl FfxMain for InputTool {
    type Writer = VerifiedMachineWriter<InputResult>;

    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> FhoResult<()> {
        // Validate the input before looking up the emulator. Invalid input never results in a
        // partially delivered sequence.
        let steps = build_steps(&self.cmd)?;
        let mut engine = self.get_engine().await?;
        send_steps(&mut *engine, &steps, writer).await
    }
}

impl InputTool {
    async fn get_engine(&self) -> FhoResult<Box<dyn EmulatorEngine>> {
        let instance_dir: PathBuf =
            self.context.get(EMU_INSTANCE_ROOT_DIR).map_err(|e| bug!("{e}"))?;
        let emu_instances = EmulatorInstances::new(instance_dir);
        let builder = EngineBuilder::new(&self.context, emu_instances);

        let mut name = self.cmd.name.clone();
        builder
            .get_engine_by_name(&mut name)
            .map_err(|e| user_error!("Failed to get emulator engine: {e}"))?
            .ok_or_else(|| match &self.cmd.name {
                Some(name) => user_error!(
                    "Could not find emulator instance '{name}'. \
                    See the available instances with `ffx emu list`."
                ),
                None => user_error!(
                    "Could not find a running emulator instance. \
                    Start one with `ffx emu start`, or see the instances with `ffx emu list`."
                ),
            })
    }
}

/// Sends `steps` to `engine` and reports the result.
async fn send_steps(
    engine: &mut dyn EmulatorEngine,
    steps: &[InputStep],
    mut writer: VerifiedMachineWriter<InputResult>,
) -> FhoResult<()> {
    engine.send_input(steps).await?;

    let result = InputResult {
        instance: engine.emu_config().runtime.name.clone(),
        steps_sent: steps.len(),
        events_sent: steps.iter().map(|step| step.events.len()).sum(),
    };
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    let message = format!(
        "Sent {} input event{} in {} step{} to {}.",
        result.events_sent,
        plural(result.events_sent),
        result.steps_sent,
        plural(result.steps_sent),
        result.instance
    );
    writer.machine_or(&result, message).map_err(|e| bug!("{e}"))?;
    Ok(())
}

/// Builds the steps for the given command. Validates all of the input first.
fn build_steps(cmd: &InputCommand) -> FhoResult<Vec<InputStep>> {
    let interval = Duration::from_millis(cmd.interval);
    match &cmd.subcommand {
        InputSubCommand::Key(key) => key_steps(key, interval),
        InputSubCommand::Text(text) => text_steps(text, interval),
        InputSubCommand::Raw(raw) => raw_steps(raw),
    }
}

/// Steps that press `keys` in order. Only the last press is followed by `delay_after`.
fn press_steps(keys: &[&'static str], delay_after: Duration) -> Vec<InputStep> {
    keys.iter()
        .enumerate()
        .map(|(index, &qcode)| InputStep {
            events: vec![InputEvent::Key { qcode, state: PressState::Down }],
            delay_after: if index + 1 == keys.len() { delay_after } else { Duration::ZERO },
        })
        .collect()
}

/// Steps that release `keys` in reverse order. Only the last release is followed by
/// `delay_after`.
fn release_steps(keys: &[&'static str], delay_after: Duration) -> Vec<InputStep> {
    keys.iter()
        .rev()
        .enumerate()
        .map(|(index, &qcode)| InputStep {
            events: vec![InputEvent::Key { qcode, state: PressState::Up }],
            delay_after: if index + 1 == keys.len() { delay_after } else { Duration::ZERO },
        })
        .collect()
}

fn key_steps(cmd: &KeyCommand, interval: Duration) -> FhoResult<Vec<InputStep>> {
    let parse_combo = |combo: &str| keymap::parse_combo(combo).map_err(|e| user_error!("{e}"));
    match (cmd.combos.is_empty(), &cmd.down, &cmd.up) {
        (false, None, None) => {
            // Validate all combos before generating any steps.
            let combos =
                cmd.combos.iter().map(|combo| parse_combo(combo)).collect::<FhoResult<Vec<_>>>()?;
            let key_duration = Duration::from_millis(cmd.key_duration);
            let mut steps = Vec::new();
            for keys in combos {
                steps.extend(press_steps(&keys, key_duration));
                steps.extend(release_steps(&keys, interval));
            }
            Ok(steps)
        }
        (true, Some(combo), None) => Ok(press_steps(&parse_combo(combo)?, Duration::ZERO)),
        (true, None, Some(combo)) => Ok(release_steps(&parse_combo(combo)?, Duration::ZERO)),
        (true, None, None) => return_user_error!(
            "Specify the keys to press, e.g. `ffx emu input key a`, or use --down or --up."
        ),
        _ => return_user_error!("--down, --up and positional key combos can't be combined."),
    }
}

fn text_steps(cmd: &TextCommand, interval: Duration) -> FhoResult<Vec<InputStep>> {
    if cmd.text.is_empty() {
        return_user_error!("The text to type is empty.");
    }
    // Validate all characters before generating any steps.
    let mut keys = Vec::new();
    for (position, character) in cmd.text.chars().enumerate() {
        let Some(key) = keymap::ascii_to_key(character) else {
            return_user_error!(
                "Character {character:?} at position {position} can't be typed. `text` accepts \
                only printable ASCII (0x20-0x7E). Use `ffx emu input key` for keys such as ret or \
                tab."
            );
        };
        keys.push(key);
    }

    let key_duration = Duration::from_millis(cmd.key_duration);
    let mut steps = Vec::new();
    for key in keys {
        let keys = if key.needs_shift { vec!["shift", key.qcode] } else { vec![key.qcode] };
        steps.extend(press_steps(&keys, key_duration));
        steps.extend(release_steps(&keys, interval));
    }
    Ok(steps)
}

fn raw_steps(cmd: &RawCommand) -> FhoResult<Vec<InputStep>> {
    let value: serde_json::Value =
        serde_json::from_str(&cmd.json).map_err(|e| user_error!("Invalid JSON for `raw`: {e}"))?;
    let serde_json::Value::Array(events) = value else {
        return_user_error!("`raw` expects a JSON array of QMP InputEvent objects.");
    };
    if events.is_empty() {
        return_user_error!("`raw` expects at least one event.");
    }
    if let Some(event) = events.iter().find(|e| !e.is_object()) {
        return_user_error!("`raw` event is not a JSON object: {event}");
    }
    Ok(vec![InputStep {
        events: events.into_iter().map(InputEvent::Raw).collect(),
        delay_after: Duration::ZERO,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use emulator_instance::EmulatorConfiguration;
    use ffx_emulator_config::BoxFuture;
    use ffx_writer::{Format, TestBuffers};

    const INTERVAL: Duration = Duration::from_millis(20);
    const ZERO: Duration = Duration::ZERO;

    fn cmd(subcommand: InputSubCommand) -> InputCommand {
        InputCommand { name: None, interval: 20, subcommand }
    }

    fn raw_cmd(json: &str) -> InputCommand {
        cmd(InputSubCommand::Raw(RawCommand { json: json.into() }))
    }

    fn key_cmd(combos: &[&str]) -> InputCommand {
        cmd(InputSubCommand::Key(KeyCommand {
            combos: combos.iter().map(|combo| combo.to_string()).collect(),
            down: None,
            up: None,
            key_duration: 20,
        }))
    }

    fn text_cmd(text: &str, key_duration: u64) -> InputCommand {
        cmd(InputSubCommand::Text(TextCommand { text: text.into(), key_duration }))
    }

    fn key(qcode: &'static str, state: PressState, delay_after: Duration) -> InputStep {
        InputStep { events: vec![InputEvent::Key { qcode, state }], delay_after }
    }

    fn err_string(result: FhoResult<Vec<InputStep>>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn test_raw() {
        let command = raw_cmd(
            r#"[{"type":"rel","data":{"axis":"x","value":7}},
                {"type":"abs","data":{"axis":"y","value":100}}]"#,
        );
        assert_eq!(
            build_steps(&command).unwrap(),
            vec![InputStep {
                events: vec![
                    InputEvent::Raw(
                        serde_json::json!({"type":"rel","data":{"axis":"x","value":7}})
                    ),
                    InputEvent::Raw(
                        serde_json::json!({"type":"abs","data":{"axis":"y","value":100}})
                    ),
                ],
                delay_after: ZERO,
            }]
        );
    }

    #[test]
    fn test_raw_errors() {
        for (json, expected) in [
            ("not json", "Invalid JSON"),
            ("{\"type\":\"rel\"}", "JSON array"),
            ("[]", "at least one"),
            ("[1]", "not a JSON object: 1"),
        ] {
            let err = err_string(build_steps(&raw_cmd(json)));
            assert!(err.contains(expected), "{json}: {err}");
        }
    }

    #[test]
    fn test_key_single() {
        assert_eq!(
            build_steps(&key_cmd(&["a"])).unwrap(),
            vec![key("a", PressState::Down, INTERVAL), key("a", PressState::Up, INTERVAL)]
        );
    }

    #[test]
    fn test_key_chord_and_sequence() {
        let mut command = key_cmd(&["ctrl+alt+delete", "ret"]);
        command.interval = 50;
        let InputSubCommand::Key(key_command) = &mut command.subcommand else { panic!() };
        key_command.key_duration = 100;
        let hold = Duration::from_millis(100);
        let gap = Duration::from_millis(50);
        assert_eq!(
            build_steps(&command).unwrap(),
            vec![
                key("ctrl", PressState::Down, ZERO),
                key("alt", PressState::Down, ZERO),
                key("delete", PressState::Down, hold),
                key("delete", PressState::Up, ZERO),
                key("alt", PressState::Up, ZERO),
                key("ctrl", PressState::Up, gap),
                key("ret", PressState::Down, hold),
                key("ret", PressState::Up, gap),
            ]
        );
    }

    #[test]
    fn test_key_down_up() {
        let down = cmd(InputSubCommand::Key(KeyCommand {
            combos: vec![],
            down: Some("ctrl+shift".into()),
            up: None,
            key_duration: 20,
        }));
        assert_eq!(
            build_steps(&down).unwrap(),
            vec![key("ctrl", PressState::Down, ZERO), key("shift", PressState::Down, ZERO)]
        );
        let up = cmd(InputSubCommand::Key(KeyCommand {
            combos: vec![],
            down: None,
            up: Some("ctrl+shift".into()),
            key_duration: 20,
        }));
        assert_eq!(
            build_steps(&up).unwrap(),
            vec![key("shift", PressState::Up, ZERO), key("ctrl", PressState::Up, ZERO)]
        );
    }

    #[test]
    fn test_key_mode_errors() {
        assert!(err_string(build_steps(&key_cmd(&[]))).contains("Specify the keys"));

        let both = cmd(InputSubCommand::Key(KeyCommand {
            combos: vec![],
            down: Some("a".into()),
            up: Some("a".into()),
            key_duration: 20,
        }));
        assert!(err_string(build_steps(&both)).contains("can't be combined"));

        let mixed = cmd(InputSubCommand::Key(KeyCommand {
            combos: vec!["a".into()],
            down: Some("b".into()),
            up: None,
            key_duration: 20,
        }));
        assert!(err_string(build_steps(&mixed)).contains("can't be combined"));
    }

    #[test]
    fn test_key_unknown_name_rejected_before_any_step() {
        // The first combo is valid, but the second isn't. No steps are generated.
        let err = err_string(build_steps(&key_cmd(&["a", "ctrl+enter"])));
        assert!(err.contains("unknown key name 'enter'"), "{err}");
    }

    #[test]
    fn test_text() {
        let hold = Duration::from_millis(30);
        assert_eq!(
            build_steps(&text_cmd("Hi!", 30)).unwrap(),
            vec![
                key("shift", PressState::Down, ZERO),
                key("h", PressState::Down, hold),
                key("h", PressState::Up, ZERO),
                key("shift", PressState::Up, INTERVAL),
                key("i", PressState::Down, hold),
                key("i", PressState::Up, INTERVAL),
                key("shift", PressState::Down, ZERO),
                key("1", PressState::Down, hold),
                key("1", PressState::Up, ZERO),
                key("shift", PressState::Up, INTERVAL),
            ]
        );
    }

    #[test]
    fn test_text_space() {
        assert_eq!(
            build_steps(&text_cmd(" ", 20)).unwrap(),
            vec![key("spc", PressState::Down, INTERVAL), key("spc", PressState::Up, INTERVAL)]
        );
    }

    #[test]
    fn test_text_errors() {
        assert!(err_string(build_steps(&text_cmd("", 20))).contains("empty"));
        for text in ["ab\n", "tab\there", "caf\u{e9}", "\u{1F600}"] {
            let err = err_string(build_steps(&text_cmd(text, 20)));
            assert!(err.contains("printable ASCII"), "{text:?}: {err}");
        }
    }

    struct StubEngine {
        config: EmulatorConfiguration,
        sent: Vec<InputStep>,
        /// If set, `send_input` fails with this message.
        error: Option<&'static str>,
    }

    impl StubEngine {
        fn new(error: Option<&'static str>) -> Self {
            let mut config = EmulatorConfiguration::default();
            config.runtime.name = "fuchsia-emulator".into();
            Self { config, sent: vec![], error }
        }
    }

    impl EmulatorEngine for StubEngine {
        fn emu_config(&self) -> &EmulatorConfiguration {
            &self.config
        }
        fn send_input<'a>(&'a mut self, steps: &'a [InputStep]) -> BoxFuture<'a, FhoResult<()>> {
            Box::pin(async move {
                if let Some(error) = self.error {
                    return_user_error!("{error}");
                }
                self.sent.extend_from_slice(steps);
                Ok(())
            })
        }
    }

    const RAW_TWO_EVENTS: &str = r#"[{"type":"rel","data":{"axis":"x","value":1}},
                                     {"type":"rel","data":{"axis":"y","value":2}}]"#;

    #[fuchsia::test]
    async fn test_send_steps_human_output() {
        let steps = build_steps(&raw_cmd(RAW_TWO_EVENTS)).unwrap();
        let mut engine = StubEngine::new(None);
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<InputResult>::new_test(None, &buffers);
        send_steps(&mut engine, &steps, writer).await.unwrap();
        assert_eq!(engine.sent, steps);
        let (stdout, stderr) = buffers.into_strings();
        assert_eq!(stdout, "Sent 2 input events in 1 step to fuchsia-emulator.\n");
        assert!(stderr.is_empty());
    }

    #[fuchsia::test]
    async fn test_send_steps_machine_output() {
        let steps = build_steps(&raw_cmd(RAW_TWO_EVENTS)).unwrap();
        let mut engine = StubEngine::new(None);
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<InputResult>::new_test(Some(Format::Json), &buffers);
        send_steps(&mut engine, &steps, writer).await.unwrap();
        let (stdout, _) = buffers.into_strings();
        let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"instance": "fuchsia-emulator", "steps_sent": 1, "events_sent": 2})
        );
        VerifiedMachineWriter::<InputResult>::verify_schema(&value).unwrap();
    }

    #[fuchsia::test]
    async fn test_send_steps_engine_error() {
        let steps = build_steps(&raw_cmd(RAW_TWO_EVENTS)).unwrap();
        let mut engine = StubEngine::new(Some("unsupported engine"));
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<InputResult>::new_test(None, &buffers);
        let err = send_steps(&mut engine, &steps, writer).await.unwrap_err().to_string();
        assert_eq!(err, "unsupported engine");
        let (stdout, _) = buffers.into_strings();
        assert!(stdout.is_empty(), "{stdout}");
    }

    #[fuchsia::test]
    async fn test_get_engine_no_instance() {
        let env = ffx_config::test_init().unwrap();
        let tool = InputTool { cmd: raw_cmd(RAW_TWO_EVENTS), context: env.context.clone() };
        let err = tool.get_engine().await.err().expect("expected an error").to_string();
        assert!(err.contains("Could not find a running emulator instance"), "{err}");
        assert!(err.contains("ffx emu start"), "{err}");
    }

    #[fuchsia::test]
    async fn test_get_engine_unknown_name() {
        let env = ffx_config::test_init().unwrap();
        let mut cmd = raw_cmd(RAW_TWO_EVENTS);
        cmd.name = Some("no-such-emulator".into());
        let tool = InputTool { cmd, context: env.context.clone() };
        let err = tool.get_engine().await.err().expect("expected an error").to_string();
        assert!(err.contains("'no-such-emulator'"), "{err}");
    }
}
