// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_core::ffx_command;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "snapshot",
    description = "Save, load, list and delete snapshots of a Fuchsia emulator.",
    example = "Start an emulator which supports snapshots, and save one once it has booted:
  $ ffx emu start --engine qemu --enable-snapshots
  $ ffx emu snapshot save booted

Roll the running emulator back to that snapshot:
  $ ffx emu snapshot load booted

Stop the emulator, then later resume from the snapshot instead of booting:
  $ ffx emu stop --persist
  $ ffx emu start --from-snapshot booted",
    note = "A snapshot captures the complete state of a running emulator: the guest's
memory, CPUs, devices and disk.

Snapshots are only supported by the \"qemu\" engine, and only for emulators
which were started with `ffx emu start --enable-snapshots`.

Snapshots are stored in the emulator's disk image, which is part of the
instance's working directory. Use `ffx emu stop --persist` to keep them when
stopping the emulator; otherwise they are deleted along with the rest of the
instance."
)]
pub struct SnapshotCommand {
    #[argh(subcommand)]
    pub subcommand: SnapshotSubCommand,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum SnapshotSubCommand {
    Delete(SnapshotDeleteCommand),
    List(SnapshotListCommand),
    Load(SnapshotLoadCommand),
    Save(SnapshotSaveCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "save",
    description = "Save a snapshot of a running Fuchsia emulator.",
    example = "ffx emu snapshot save booted
ffx emu snapshot save booted --name fuchsia-emulator",
    note = "If a snapshot with the same name already exists, it is replaced.

The emulator is paused while its state is written to disk, which can take
several seconds."
)]
pub struct SnapshotSaveCommand {
    /// name to save the snapshot under. May contain letters, digits, '.', '_'
    /// and '-', and must start with a letter or digit.
    #[argh(positional)]
    pub snapshot: String,

    /// name of the emulator to snapshot, as specified to the start command.
    /// See a list of available instances by running `ffx emu list`. If no name
    /// is specified, and only one emulator is running, it will be selected.
    #[argh(option)]
    pub name: Option<String>,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "load",
    description = "Restore a running Fuchsia emulator to a previously saved snapshot.",
    example = "ffx emu snapshot load booted
ffx emu snapshot load booted --name fuchsia-emulator",
    note = "The current state of the emulator, including any changes made to its disk
since the snapshot was saved, is discarded.

Connections which the guest had open do not survive a load: an open
`ffx target ssh` session hangs and has to be started again, while new ffx
commands reconnect to the emulator on their own. The guest's wall clock goes
back to the time at which the snapshot was saved.

To resume a stopped emulator from a snapshot, use
`ffx emu start --from-snapshot <snapshot>` instead."
)]
pub struct SnapshotLoadCommand {
    /// name of the snapshot to load, as specified to `ffx emu snapshot save`.
    #[argh(positional)]
    pub snapshot: String,

    /// name of the emulator to restore, as specified to the start command.
    /// See a list of available instances by running `ffx emu list`. If no name
    /// is specified, and only one emulator is running, it will be selected.
    #[argh(option)]
    pub name: Option<String>,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "list",
    description = "List the snapshots saved for a Fuchsia emulator.",
    example = "ffx emu snapshot list
ffx emu snapshot list --name fuchsia-emulator",
    note = "The emulator does not have to be running. The snapshots of an emulator which
was stopped with `ffx emu stop --persist` are the ones it can be resumed from
with `ffx emu start --from-snapshot <snapshot>`."
)]
pub struct SnapshotListCommand {
    /// name of the emulator to list the snapshots of, as specified to the
    /// start command. See a list of available instances by running
    /// `ffx emu list`. If no name is specified, and only one emulator is
    /// running, it will be selected.
    #[argh(option)]
    pub name: Option<String>,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "delete",
    description = "Delete snapshots of a Fuchsia emulator.",
    example = "  $ ffx emu snapshot delete booted
  $ ffx emu snapshot delete booted --name fuchsia-emulator
  $ ffx emu snapshot delete --all",
    note = "The emulator does not have to be running. Deleting a snapshot frees the disk
space which it took up in the emulator's disk image. --all deletes all of the
emulator's snapshots, and keeps the emulator instance."
)]
pub struct SnapshotDeleteCommand {
    /// name of the snapshot to delete, as specified to `ffx emu snapshot save`.
    #[argh(positional)]
    pub snapshot: Option<String>,

    /// delete all of the emulator's snapshots.
    #[argh(switch)]
    pub all: bool,

    /// name of the emulator to delete the snapshot of, as specified to the
    /// start command. See a list of available instances by running
    /// `ffx emu list`. If no name is specified, and only one emulator is
    /// running, it will be selected.
    #[argh(option)]
    pub name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD_NAME: &[&str] = &["snapshot"];

    #[test]
    fn test_save_args() {
        let cmd = SnapshotCommand::from_args(CMD_NAME, &["save", "booted"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Save(SnapshotSaveCommand {
                snapshot: "booted".to_string(),
                name: None
            })
        );
    }

    #[test]
    fn test_save_args_with_emulator_name() {
        let cmd =
            SnapshotCommand::from_args(CMD_NAME, &["save", "booted", "--name", "fuchsia-emulator"])
                .unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Save(SnapshotSaveCommand {
                snapshot: "booted".to_string(),
                name: Some("fuchsia-emulator".to_string())
            })
        );
    }

    #[test]
    fn test_save_args_emulator_name_is_not_positional() {
        assert!(
            SnapshotCommand::from_args(CMD_NAME, &["save", "booted", "fuchsia-emulator"]).is_err()
        );
    }

    #[test]
    fn test_save_args_missing_snapshot() {
        assert!(SnapshotCommand::from_args(CMD_NAME, &["save"]).is_err());
    }

    #[test]
    fn test_load_args() {
        let cmd = SnapshotCommand::from_args(CMD_NAME, &["load", "booted"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Load(SnapshotLoadCommand {
                snapshot: "booted".to_string(),
                name: None
            })
        );
    }

    #[test]
    fn test_load_args_with_emulator_name() {
        let cmd =
            SnapshotCommand::from_args(CMD_NAME, &["load", "booted", "--name", "fuchsia-emulator"])
                .unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Load(SnapshotLoadCommand {
                snapshot: "booted".to_string(),
                name: Some("fuchsia-emulator".to_string())
            })
        );
    }

    #[test]
    fn test_load_args_emulator_name_is_not_positional() {
        assert!(
            SnapshotCommand::from_args(CMD_NAME, &["load", "booted", "fuchsia-emulator"]).is_err()
        );
    }

    #[test]
    fn test_load_args_missing_snapshot() {
        assert!(SnapshotCommand::from_args(CMD_NAME, &["load"]).is_err());
    }

    #[test]
    fn test_list_args() {
        let cmd = SnapshotCommand::from_args(CMD_NAME, &["list"]).unwrap();
        assert_eq!(cmd.subcommand, SnapshotSubCommand::List(SnapshotListCommand { name: None }));
    }

    #[test]
    fn test_list_args_with_emulator_name() {
        let cmd =
            SnapshotCommand::from_args(CMD_NAME, &["list", "--name", "fuchsia-emulator"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::List(SnapshotListCommand {
                name: Some("fuchsia-emulator".to_string())
            })
        );
    }

    #[test]
    fn test_list_args_emulator_name_is_not_positional() {
        assert!(SnapshotCommand::from_args(CMD_NAME, &["list", "fuchsia-emulator"]).is_err());
    }

    #[test]
    fn test_delete_args() {
        let cmd = SnapshotCommand::from_args(CMD_NAME, &["delete", "booted"]).unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Delete(SnapshotDeleteCommand {
                snapshot: Some("booted".to_string()),
                all: false,
                name: None
            })
        );
    }

    #[test]
    fn test_delete_args_with_emulator_name() {
        let cmd = SnapshotCommand::from_args(
            CMD_NAME,
            &["delete", "booted", "--name", "fuchsia-emulator"],
        )
        .unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Delete(SnapshotDeleteCommand {
                snapshot: Some("booted".to_string()),
                all: false,
                name: Some("fuchsia-emulator".to_string())
            })
        );
    }

    #[test]
    fn test_delete_args_emulator_name_is_not_positional() {
        assert!(
            SnapshotCommand::from_args(CMD_NAME, &["delete", "booted", "fuchsia-emulator"])
                .is_err()
        );
    }

    #[test]
    fn test_delete_all_args() {
        let cmd = SnapshotCommand::from_args(
            CMD_NAME,
            &["delete", "--all", "--name", "fuchsia-emulator"],
        )
        .unwrap();
        assert_eq!(
            cmd.subcommand,
            SnapshotSubCommand::Delete(SnapshotDeleteCommand {
                snapshot: None,
                all: true,
                name: Some("fuchsia-emulator".to_string())
            })
        );
    }

    #[test]
    fn test_missing_subcommand() {
        assert!(SnapshotCommand::from_args(CMD_NAME, &[]).is_err());
    }
}
