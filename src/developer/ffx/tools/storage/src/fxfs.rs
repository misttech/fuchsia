// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use argh::{ArgsInfo, FromArgs};
use fdomain_fuchsia_fxfs::{DebugProxy, ProfileIdentifier, ProfileManagerProxy};
use ffx_writer::SimpleWriter;
use fho::{Deferred, Error, Result, return_user_error, user_error};
use zx_status::Status;

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "compact",
    example = "ffx storage fxfs compact",
    description = "Forces a (blocking) compaction of all layer files."
)]
pub struct CompactSubCommand {}

// TODO(https://fxbug.dev/507875809): Update delete_profile to support node associated profiles.
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "delete_profile",
    example = "ffx storage fxfs delete_profile",
    description = "Deletes a profile from a named unlocked volume. Fails during active profile \
        record and/or replay."
)]
pub struct DeleteProfileSubCommand {
    #[argh(positional)]
    volume: String,
    #[argh(positional)]
    profile: String,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "record_and_replay_profile",
    example = "ffx storage fxfs record_and_replay_profile --volume data startup 60 ",
    description = "Starts recording a for a named unlocked volume to run for a limited number of \
        time. If a profile exists on the volume with the given name, then it will also begin \
        replaying it. If no volume is given, then all unlocked volumes are activated for recording \
        and replay. Fails during active profile recording and/or replay."
)]
pub struct RecordAndReplayProfileSubCommand {
    #[argh(positional)]
    profile: String,
    #[argh(positional)]
    duration_secs: u32,
    #[argh(option, short = 'v')]
    /// the volume to affect instead of all unlocked volumes.
    volume: Option<String>,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "replay_xor_record_profile",
    example = "ffx storage fxfs replay_xor_record_profile --volume data startup 60 ",
    description = "Replays a profile for a named unlocked volume if one exists, otherwise it \
        starts recording one. The identifier is either a uint node id, or 64 character hex \
        encoding of blob root hash, for the latter specify -b. Fails during active profile \
        recording and/or replay."
)]
pub struct ReplayXorRecordProfileSubCommand {
    #[argh(positional)]
    identifier: String,
    #[argh(positional)]
    duration_secs: u32,
    #[argh(option, short = 'v')]
    /// the volume to affect.
    volume: String,
    #[argh(switch, short = 'b')]
    /// whether the identifier is a 64-character hex blob hash instead of an object ID.
    blob: bool,
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "stop_profile",
    example = "ffx storage fxfs stop_profile",
    description = "Blocks while stopping all profile recording and/or replay activity."
)]
pub struct StopProfileSubCommand {}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand)]
pub enum FxfsSubCommand {
    Compact(CompactSubCommand),
    DeleteProfile(DeleteProfileSubCommand),
    RecordAndReplayProfile(RecordAndReplayProfileSubCommand),
    ReplayXorRecordProfile(ReplayXorRecordProfileSubCommand),
    StopProfile(StopProfileSubCommand),
}

#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "fxfs", description = "Interact with fxfs instances.")]
pub struct FxfsCommand {
    #[argh(subcommand)]
    subcommand: FxfsSubCommand,
}

pub async fn handle_cmd(
    cmd: FxfsCommand,
    _writer: SimpleWriter,
    debug_proxy: Deferred<DebugProxy>,
    profile_proxy: Deferred<ProfileManagerProxy>,
) -> Result<()> {
    match cmd.subcommand {
        FxfsSubCommand::Compact(_) => {
            debug_proxy
                .await?
                .compact()
                .await
                .map_err(|e| user_error!(e))?
                .map_err(Error::ExitWithCode)?;
        }
        FxfsSubCommand::DeleteProfile(args) => {
            profile_proxy
                .await?
                .delete(&args.volume, &args.profile)
                .await
                .map_err(|e| user_error!(e))?
                .map_err(Error::ExitWithCode)?;
        }
        FxfsSubCommand::RecordAndReplayProfile(args) => {
            profile_proxy
                .await?
                .record_and_replay(args.volume.as_deref(), &args.profile, args.duration_secs)
                .await
                .map_err(|e| user_error!(e))?
                .map_err(|e| user_error!(Status::err_from_raw(e)))?;
        }
        FxfsSubCommand::ReplayXorRecordProfile(args) => {
            let identifier = if args.blob {
                let mut array = [0u8; 32];
                let bytes =
                    hex::decode(&args.identifier).map_err(|_| user_error!("Invalid hex string"))?;
                if bytes.len() != 32 {
                    return_user_error!("Blob hash must be 32 bytes (64 hex characters)");
                }
                array.copy_from_slice(&bytes);
                ProfileIdentifier::BlobHash(array)
            } else {
                let oid =
                    args.identifier.parse::<u64>().map_err(|_| user_error!("Invalid object ID"))?;
                ProfileIdentifier::ObjectId(oid)
            };
            profile_proxy
                .await?
                .record_xor_replay(&args.volume, &identifier, args.duration_secs)
                .await
                .map_err(|e| user_error!(e))?
                .map_err(|e| user_error!(Status::err_from_raw(e)))?;
        }
        FxfsSubCommand::StopProfile(_) => {
            profile_proxy
                .await?
                .stop_tasks()
                .await
                .map_err(|e| user_error!(e))?
                .map_err(Error::ExitWithCode)?;
        }
    };
    Ok(())
}
