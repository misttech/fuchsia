// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use emulator_instance::EmulatorInstances;
use ffx_config::EnvironmentContext;
use ffx_config::keys::EMU_INSTANCE_ROOT_DIR;
use ffx_emulator_config::{EmulatorEngine, SnapshotInfo};
use ffx_emulator_engines::EngineBuilder;
use ffx_emulator_snapshot_args::{SnapshotCommand, SnapshotSubCommand};
use ffx_writer::VerifiedMachineWriter;
use fho::{FfxMain, FfxTool, Result, bug, user_error};
use schemars::JsonSchema;
use serde::Serialize;
use std::path::PathBuf;

/// The result of a successful `ffx emu snapshot` subcommand.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SnapshotOutput {
    /// The snapshots which are saved for the emulator.
    List { snapshots: Vec<SnapshotInfo> },
    /// The emulator was restored to the named snapshot.
    Loaded { name: String },
    /// The emulator's state was saved under the given name.
    Saved { name: String },
}

/// Sub-sub tool for `emu snapshot`
#[derive(FfxTool)]
#[target(None)]
pub struct EmuSnapshotTool {
    #[command]
    cmd: SnapshotCommand,
    context: EnvironmentContext,
}

fho::embedded_plugin!(EmuSnapshotTool);

impl FfxMain for EmuSnapshotTool {
    type Writer = VerifiedMachineWriter<SnapshotOutput>;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let mut engine = self.get_engine()?;
        run_subcommand(&self.cmd.subcommand, engine.as_mut(), &mut writer).await
    }
}

impl EmuSnapshotTool {
    /// Returns the engine of the emulator instance which the command applies to.
    fn get_engine(&self) -> Result<Box<dyn EmulatorEngine>> {
        let instance_dir: PathBuf =
            self.context.get(EMU_INSTANCE_ROOT_DIR).map_err(|e| bug!("{e}"))?;
        let builder = EngineBuilder::new(&self.context, EmulatorInstances::new(instance_dir));

        let mut name = match &self.cmd.subcommand {
            SnapshotSubCommand::List(cmd) => cmd.name.clone(),
            SnapshotSubCommand::Load(cmd) => cmd.name.clone(),
            SnapshotSubCommand::Save(cmd) => cmd.name.clone(),
        };
        builder
            .get_engine_by_name(&mut name)
            .map_err(|e| user_error!("Failed to get emulator engine: {e}"))?
            .ok_or_else(|| match name {
                Some(name) => user_error!("Emulator instance '{name}' not found."),
                None => user_error!("Could not find emulator instance."),
            })
    }
}

/// Runs the given subcommand against the emulator `engine`, and reports the result to `writer`.
async fn run_subcommand(
    subcommand: &SnapshotSubCommand,
    engine: &mut dyn EmulatorEngine,
    writer: &mut VerifiedMachineWriter<SnapshotOutput>,
) -> Result<()> {
    match subcommand {
        SnapshotSubCommand::Save(cmd) => {
            engine.save_snapshot(&cmd.snapshot).await?;
            writer
                .machine_or_else(&SnapshotOutput::Saved { name: cmd.snapshot.clone() }, || {
                    format!("Saved snapshot '{}'.", cmd.snapshot)
                })?;
        }
        SnapshotSubCommand::Load(cmd) => {
            engine.load_snapshot(&cmd.snapshot).await?;
            writer
                .machine_or_else(&SnapshotOutput::Loaded { name: cmd.snapshot.clone() }, || {
                    format!("Loaded snapshot '{}'.", cmd.snapshot)
                })?;
        }
        SnapshotSubCommand::List(_) => {
            let snapshots = engine.list_snapshots().await?;
            let text = format_snapshots(&snapshots);
            writer.machine_or(&SnapshotOutput::List { snapshots }, text)?;
        }
    }
    Ok(())
}

/// Formats the snapshots as a table to display to the user.
fn format_snapshots(snapshots: &[SnapshotInfo]) -> String {
    if snapshots.is_empty() {
        return "No snapshots found.".to_string();
    }
    let name_width = snapshots.iter().map(|s| s.name.len()).fold("NAME".len(), usize::max);
    let mut lines =
        vec![format!("{:name_width$}  {:>10}  {:19}  {}", "NAME", "VM SIZE", "DATE", "VM CLOCK")];
    for snapshot in snapshots {
        lines.push(format!(
            "{:name_width$}  {:>10}  {:19}  {}",
            snapshot.name, snapshot.vm_size, snapshot.date, snapshot.vm_clock
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_emulator_config::BoxFuture;
    use ffx_emulator_snapshot_args::{
        SnapshotListCommand, SnapshotLoadCommand, SnapshotSaveCommand,
    };
    use ffx_writer::{Format, TestBuffers};

    /// Records the snapshot operations it is asked to perform, and optionally fails them.
    #[derive(Default)]
    struct StubEngine {
        saved: Vec<String>,
        loaded: Vec<String>,
        snapshots: Vec<SnapshotInfo>,
        error: Option<String>,
    }

    impl StubEngine {
        fn result(&self) -> Result<()> {
            match &self.error {
                Some(message) => Err(user_error!("{message}")),
                None => Ok(()),
            }
        }
    }

    impl EmulatorEngine for StubEngine {
        fn save_snapshot<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.result()?;
                self.saved.push(name.to_string());
                Ok(())
            })
        }

        fn load_snapshot<'a>(&'a mut self, name: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.result()?;
                self.loaded.push(name.to_string());
                Ok(())
            })
        }

        fn list_snapshots(&mut self) -> BoxFuture<'_, Result<Vec<SnapshotInfo>>> {
            Box::pin(async move {
                self.result()?;
                Ok(self.snapshots.clone())
            })
        }
    }

    fn save_command(snapshot: &str) -> SnapshotSubCommand {
        SnapshotSubCommand::Save(SnapshotSaveCommand { snapshot: snapshot.to_string(), name: None })
    }

    fn load_command(snapshot: &str) -> SnapshotSubCommand {
        SnapshotSubCommand::Load(SnapshotLoadCommand { snapshot: snapshot.to_string(), name: None })
    }

    fn list_command() -> SnapshotSubCommand {
        SnapshotSubCommand::List(SnapshotListCommand { name: None })
    }

    fn test_snapshots() -> Vec<SnapshotInfo> {
        vec![
            SnapshotInfo {
                name: "booted".to_string(),
                vm_size: "1.25 GiB".to_string(),
                date: "2026-09-30 22:16:08".to_string(),
                vm_clock: "0000:00:25.123".to_string(),
            },
            SnapshotInfo {
                name: "a-much-longer-snapshot-name".to_string(),
                vm_size: "900 MiB".to_string(),
                date: "2026-10-01 08:00:00".to_string(),
                vm_clock: "0001:02:03.004".to_string(),
            },
        ]
    }

    /// Runs the subcommand, and returns what was written to stdout.
    async fn run(
        subcommand: SnapshotSubCommand,
        engine: &mut StubEngine,
        format: Option<Format>,
    ) -> Result<String> {
        let buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<SnapshotOutput>::new_test(format, &buffers);
        run_subcommand(&subcommand, engine, &mut writer).await?;
        let (stdout, stderr) = buffers.into_strings();
        assert_eq!(stderr, "");
        Ok(stdout)
    }

    /// Checks that the machine output matches both the expected value and the schema.
    fn assert_machine_output(stdout: &str, expected: serde_json::Value) {
        let data: serde_json::Value = serde_json::from_str(stdout).expect("valid json");
        assert_eq!(data, expected);
        VerifiedMachineWriter::<SnapshotOutput>::verify_schema(&data).expect("schema matches");
    }

    #[fuchsia::test]
    async fn test_save() {
        let mut engine = StubEngine::default();
        let stdout = run(save_command("booted"), &mut engine, None).await.expect("save succeeds");
        assert_eq!(stdout, "Saved snapshot 'booted'.\n");
        assert_eq!(engine.saved, vec!["booted".to_string()]);
        assert!(engine.loaded.is_empty());
    }

    #[fuchsia::test]
    async fn test_save_machine() {
        let mut engine = StubEngine::default();
        let stdout = run(save_command("booted"), &mut engine, Some(Format::Json))
            .await
            .expect("save succeeds");
        assert_machine_output(
            &stdout,
            serde_json::json!({"type": "saved", "data": {"name": "booted"}}),
        );
        assert_eq!(engine.saved, vec!["booted".to_string()]);
    }

    #[fuchsia::test]
    async fn test_save_error() {
        let mut engine =
            StubEngine { error: Some("the engine failed".to_string()), ..Default::default() };
        let err = run(save_command("booted"), &mut engine, None).await.expect_err("save fails");
        assert!(err.to_string().contains("the engine failed"), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_load() {
        let mut engine = StubEngine::default();
        let stdout = run(load_command("booted"), &mut engine, None).await.expect("load succeeds");
        assert_eq!(stdout, "Loaded snapshot 'booted'.\n");
        assert_eq!(engine.loaded, vec!["booted".to_string()]);
        assert!(engine.saved.is_empty());
    }

    #[fuchsia::test]
    async fn test_load_machine() {
        let mut engine = StubEngine::default();
        let stdout = run(load_command("booted"), &mut engine, Some(Format::Json))
            .await
            .expect("load succeeds");
        assert_machine_output(
            &stdout,
            serde_json::json!({"type": "loaded", "data": {"name": "booted"}}),
        );
        assert_eq!(engine.loaded, vec!["booted".to_string()]);
    }

    #[fuchsia::test]
    async fn test_load_error() {
        let mut engine =
            StubEngine { error: Some("the engine failed".to_string()), ..Default::default() };
        let err = run(load_command("booted"), &mut engine, None).await.expect_err("load fails");
        assert!(err.to_string().contains("the engine failed"), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_list() {
        let mut engine = StubEngine { snapshots: test_snapshots(), ..Default::default() };
        let stdout = run(list_command(), &mut engine, None).await.expect("list succeeds");
        assert_eq!(
            stdout,
            "NAME                            VM SIZE  DATE                 VM CLOCK\n\
             booted                         1.25 GiB  2026-09-30 22:16:08  0000:00:25.123\n\
             a-much-longer-snapshot-name     900 MiB  2026-10-01 08:00:00  0001:02:03.004\n"
        );
    }

    #[fuchsia::test]
    async fn test_list_empty() {
        let mut engine = StubEngine::default();
        let stdout = run(list_command(), &mut engine, None).await.expect("list succeeds");
        assert_eq!(stdout, "No snapshots found.\n");
    }

    #[fuchsia::test]
    async fn test_list_machine() {
        let mut engine = StubEngine { snapshots: test_snapshots(), ..Default::default() };
        let stdout =
            run(list_command(), &mut engine, Some(Format::Json)).await.expect("list succeeds");
        assert_machine_output(
            &stdout,
            serde_json::json!({
                "type": "list",
                "data": {
                    "snapshots": [
                        {
                            "name": "booted",
                            "vm_size": "1.25 GiB",
                            "date": "2026-09-30 22:16:08",
                            "vm_clock": "0000:00:25.123"
                        },
                        {
                            "name": "a-much-longer-snapshot-name",
                            "vm_size": "900 MiB",
                            "date": "2026-10-01 08:00:00",
                            "vm_clock": "0001:02:03.004"
                        }
                    ]
                }
            }),
        );
    }

    #[fuchsia::test]
    async fn test_list_empty_machine() {
        let mut engine = StubEngine::default();
        let stdout =
            run(list_command(), &mut engine, Some(Format::Json)).await.expect("list succeeds");
        assert_machine_output(
            &stdout,
            serde_json::json!({"type": "list", "data": {"snapshots": []}}),
        );
    }

    #[fuchsia::test]
    async fn test_list_error() {
        let mut engine =
            StubEngine { error: Some("the engine failed".to_string()), ..Default::default() };
        let err = run(list_command(), &mut engine, None).await.expect_err("list fails");
        assert!(err.to_string().contains("the engine failed"), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_get_engine_no_instance() {
        let env = ffx_config::test_init().unwrap();
        let tool = EmuSnapshotTool {
            cmd: SnapshotCommand { subcommand: list_command() },
            context: env.context.clone(),
        };
        let err = tool.get_engine().err().expect("no engine found");
        assert!(err.to_string().contains("Could not find emulator instance"), "{err}");
    }

    #[fuchsia::test]
    async fn test_get_engine_unknown_instance() {
        let env = ffx_config::test_init().unwrap();
        let tool = EmuSnapshotTool {
            cmd: SnapshotCommand {
                subcommand: SnapshotSubCommand::Save(SnapshotSaveCommand {
                    snapshot: "booted".to_string(),
                    name: Some("unknown-emulator".to_string()),
                }),
            },
            context: env.context.clone(),
        };
        let err = tool.get_engine().err().expect("no engine found");
        assert!(err.to_string().contains("'unknown-emulator' not found"), "{err}");
    }
}
