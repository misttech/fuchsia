// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next::fuchsia::channel::Buffer;
use fidl_next::wire::Uint64;
use fidl_next::{AsDecoderExt as _, CHUNK_SIZE, Message, Method, Request};
use fidl_next_fuchsia_boot::svc_stash::Store;
use fidl_next_fuchsia_boot::userboot::{PostBootfsFiles, PostBootfsVmo, PostStashSvc};
use fidl_next_fuchsia_debugdata::publisher::Publish;
use fuchsia_runtime::{HandleInfo, HandleType, take_startup_handle};
use std::fmt::Write as _;
use std::panic::set_hook;
use std::process::abort;
use zx::sys::{ZX_HANDLE_INVALID, zx_handle_t};
use zx::{
    Channel, EventPair, HandleRef, Koid, MonotonicInstant, NullableHandle, ObjectType, Signals,
    Vmo, WaitResult,
};
use zx_libc::sanitizer::Log;

pub use fidl_next_fuchsia_boot::natural::BootfsFileVmo;

unsafe extern "C" {
    fn dl_set_loader_service(new_svc: zx_handle_t) -> zx_handle_t;
}

/// Decoded contents of a `fuchsia.debugdata.Publisher/Publish` FIDL request.
#[derive(Debug)]
pub struct DebugDataMessage {
    pub sink: String,
    pub vmo: Vmo,
    pub token: EventPair,
}

/// Installs a panic hook that writes panic messages to the kernel debuglog via
/// `__sanitizer_log_write` before aborting.
pub fn init_test_logging() {
    set_hook(Box::new(|info| {
        let _ = writeln!(&mut Log::new(), "{info}");
        abort();
    }));
}

/// Takes the `/svc` startup handle (`PA_HND(PA_NS_DIR, 0)`).
///
/// Callers must keep the returned [`Channel`] alive for as long as
/// `zx_libc::sanitizer::publish_data` is called, because libc's preinit stores
/// an unowned borrow of this handle in `__zircon_namespace_svc`.
pub fn take_svc_channel() -> Channel {
    let handle = take_startup_handle(HandleInfo::new(HandleType::NamespaceDirectory, 0))
        .expect("missing /svc startup handle");
    assert!(!handle.is_invalid());
    Channel::from(handle)
}

/// Returns the kernel object ID of `handle`.
pub fn koid(handle: HandleRef<'_>) -> Koid {
    handle.koid().expect("failed to get koid")
}

/// Returns the peer kernel object ID (`related_koid`) of `handle`.
pub fn peer_koid(handle: HandleRef<'_>) -> Koid {
    handle.basic_info().expect("failed to get basic_info").related_koid
}

fn is_channel_readable(channel: &Channel) -> bool {
    match channel.wait_one(Signals::CHANNEL_READABLE, MonotonicInstant::INFINITE_PAST) {
        WaitResult::Ok(observed) => observed.contains(Signals::CHANNEL_READABLE),
        WaitResult::TimedOut(_) => false,
        other => panic!("unexpected wait_one result on channel: {other:?}"),
    }
}

fn read_fidl_message(channel: &Channel) -> Message<Channel> {
    let mut bytes = Vec::new();
    let mut handles = Vec::new();
    channel.read_split(&mut bytes, &mut handles).expect("failed to read FIDL message from channel");
    assert_eq!(bytes.len() % CHUNK_SIZE, 0, "FIDL message is not 8-byte aligned");
    let chunks = bytes
        .chunks_exact(CHUNK_SIZE)
        .map(|chunk| {
            Uint64(u64::from_le_bytes(chunk.try_into().expect("chunk has CHUNK_SIZE bytes")))
        })
        .collect();
    let buffer = Buffer { chunks, handles };
    Message::<Channel>::decode(buffer).expect("failed to decode FIDL header")
}

fn drain_userboot(userboot: Channel) -> (Option<Channel>, Option<Vmo>, Vec<BootfsFileVmo>) {
    let mut stash_svc = None;
    let mut bootfs_vmo = None;
    let mut entries = Vec::new();
    while is_channel_readable(&userboot) {
        let mut message = read_fidl_message(&userboot);
        let ordinal = *message.header().ordinal;
        if ordinal == PostStashSvc::ORDINAL {
            let decoded = message
                .into_decoded::<<PostStashSvc as Method>::Request>()
                .expect("failed to decode PostStashSvc request");
            let payload = Request::<PostStashSvc, Channel>::from_decoded(decoded).payload();
            stash_svc = Some(payload.stash_svc_endpoint.into_untyped());
        } else if ordinal == PostBootfsVmo::ORDINAL {
            let decoded = message
                .into_decoded::<<PostBootfsVmo as Method>::Request>()
                .expect("failed to decode PostBootfsVmo request");
            let payload = Request::<PostBootfsVmo, Channel>::from_decoded(decoded).payload();
            bootfs_vmo = Some(payload.bootfs);
        } else if ordinal == PostBootfsFiles::ORDINAL {
            let decoded = message
                .into_decoded::<<PostBootfsFiles as Method>::Request>()
                .expect("failed to decode PostBootfsFiles request");
            let payload = Request::<PostBootfsFiles, Channel>::from_decoded(decoded).payload();
            entries.extend(payload.files);
        } else {
            panic!("unexpected fuchsia.boot.Userboot ordinal: {ordinal}");
        }
    }
    (stash_svc, bootfs_vmo, entries)
}

/// Extracts the `fuchsia.boot.SvcStash` server channel from the startup
/// `fuchsia.boot.Userboot` channel (`PA_HND(PA_USER0, 0)`).
///
/// Mutually exclusive with [`bootfs_entries`].
pub fn svc_stash() -> Channel {
    let userboot: Channel = take_startup_handle(HandleInfo::new(HandleType::User0, 0))
        .expect("missing PA_USER0 startup handle")
        .into();
    assert!(!userboot.is_invalid());
    let (stash_svc, _bootfs_vmo, _entries) = drain_userboot(userboot);
    let stash_svc = stash_svc.expect("Userboot did not post a SvcStash endpoint");
    assert!(!stash_svc.is_invalid());
    stash_svc
}

/// Extracts the `BootfsFileVmo` entries posted to the startup
/// `fuchsia.boot.Userboot` channel (`PA_HND(PA_USER0, 0)`).
///
/// Mutually exclusive with [`svc_stash`].
pub fn bootfs_entries() -> Vec<BootfsFileVmo> {
    let userboot: Channel = take_startup_handle(HandleInfo::new(HandleType::User0, 0))
        .expect("missing PA_USER0 startup handle")
        .into();
    assert!(!userboot.is_invalid());

    // Close the loader service channel so userboot finishes serving loader
    // requests, posts the bootfs entries, and closes its Userboot endpoint.
    // SAFETY: `dl_set_loader_service(ZX_HANDLE_INVALID)` replaces the active
    // loader service handle with `ZX_HANDLE_INVALID` and returns ownership of
    // the previous handle.
    let old_loader = unsafe { NullableHandle::from_raw(dl_set_loader_service(ZX_HANDLE_INVALID)) };
    drop(old_loader);

    let wait_res = userboot.wait_one(Signals::CHANNEL_PEER_CLOSED, MonotonicInstant::INFINITE);
    match wait_res {
        WaitResult::Ok(signals) => assert!(signals.contains(Signals::CHANNEL_PEER_CLOSED)),
        other => panic!("unexpected wait_one result waiting for userboot peer close: {other:?}"),
    }

    let (_stash_svc, bootfs_vmo, entries) = drain_userboot(userboot);
    assert!(bootfs_vmo.is_some(), "Userboot did not post a BootfsVmo");
    assert!(
        take_startup_handle(HandleInfo::new(HandleType::BootfsVmo, 0)).is_none(),
        "PA_VMO_BOOTFS should not be passed in startup handles when Userboot protocol is used"
    );
    assert!(!entries.is_empty());
    entries
}

/// Reads stashed `/svc` server endpoints from `svc_stash`.
///
/// Returns `(svc_0, svc_1)`, where `svc_1` is the most recently stored `/svc`
/// endpoint and `svc_0` (if present) is the previously stored `/svc` endpoint.
pub fn stashed_svc(svc_stash: Channel) -> (Option<Channel>, Channel) {
    assert!(!svc_stash.is_invalid());
    let mut svcs = Vec::new();
    while is_channel_readable(&svc_stash) {
        let mut message = read_fidl_message(&svc_stash);
        assert_eq!(
            *message.header().ordinal,
            Store::ORDINAL,
            "unexpected fuchsia.boot.SvcStash ordinal"
        );
        let decoded = message
            .into_decoded::<<Store as Method>::Request>()
            .expect("failed to decode SvcStash.Store request");
        let payload = Request::<Store, Channel>::from_decoded(decoded).payload();
        svcs.push(payload.svc_endpoint.into_untyped());
    }

    assert!(!svcs.is_empty(), "no stashed svc endpoints found");
    let svc_1 = svcs.pop().expect("svcs is non-empty");
    assert!(!svc_1.is_invalid());
    let svc_0 = svcs.pop();
    if let Some(ref svc_0) = svc_0 {
        assert!(!svc_0.is_invalid());
    }
    assert!(svcs.is_empty(), "expected at most 2 stashed svc endpoints");
    (svc_0, svc_1)
}

/// Reads the most recently stashed `/svc` server endpoint from `svc_stash`.
pub fn single_stashed_svc(svc_stash: Channel) -> Channel {
    let (_dropped, svc) = stashed_svc(svc_stash);
    svc
}

/// Reads a pipelined `fuchsia.debugdata.Publisher/Publish` message from `svc`.
pub fn debug_data_message(svc: &Channel) -> DebugDataMessage {
    match svc.wait_one(Signals::CHANNEL_READABLE, MonotonicInstant::INFINITE_PAST) {
        WaitResult::Ok(observed) => assert!(observed.contains(Signals::CHANNEL_READABLE)),
        other => panic!("expected svc channel to be readable, got: {other:?}"),
    }

    // Read the directory open request carrying the server end of the Publisher protocol.
    let mut open_bytes = Vec::new();
    let mut open_handles = Vec::new();
    svc.read_split(&mut open_bytes, &mut open_handles)
        .expect("failed to read open request from svc channel");
    assert!(!open_bytes.is_empty());
    assert_eq!(open_handles.len(), 1);

    let debug_data_handle = open_handles.pop().expect("verified 1 handle");
    let info = debug_data_handle.basic_info().expect("failed to get basic_info on handle");
    assert_eq!(info.object_type, ObjectType::CHANNEL);

    let debug_data = Channel::from(debug_data_handle);
    match debug_data.wait_one(Signals::CHANNEL_READABLE, MonotonicInstant::INFINITE_PAST) {
        WaitResult::Ok(observed) => assert!(observed.contains(Signals::CHANNEL_READABLE)),
        other => panic!("expected debug_data channel to be readable, got: {other:?}"),
    }

    let mut message = read_fidl_message(&debug_data);
    assert_eq!(
        *message.header().ordinal,
        Publish::ORDINAL,
        "unexpected fuchsia.debugdata.Publisher ordinal"
    );
    let decoded = message
        .into_decoded::<<Publish as Method>::Request>()
        .expect("failed to decode Publisher.Publish request");
    let payload = Request::<Publish, Channel>::from_decoded(decoded).payload();
    assert!(!payload.data.is_invalid());
    assert!(!payload.vmo_token.is_invalid());

    DebugDataMessage { sink: payload.data_sink, vmo: payload.data, token: payload.vmo_token }
}

/// Calls `callable` on `channel` while `channel` has readable messages and
/// `callable` returns `true` to keep going.
pub fn on_each_message(channel: &Channel, mut callable: impl FnMut(&Channel) -> bool) {
    while is_channel_readable(channel) {
        if !callable(channel) {
            return;
        }
    }
}
