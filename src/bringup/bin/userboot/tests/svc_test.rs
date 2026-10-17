// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use helper::{
    debug_data_message, init_test_logging, koid, on_each_message, peer_koid, single_stashed_svc,
    svc_stash, take_svc_channel,
};
use std::ffi::CStr;
use zx::sys::ZX_KOID_INVALID;
use zx::{Channel, EventPair, Koid, MonotonicInstant, Signals, Vmo, WaitResult};
use zx_libc::sanitizer::publish_data;

struct SvcSingleProcessTest {
    local_svc: Channel,
    stashed_svc: Channel,
}

impl SvcSingleProcessTest {
    fn new() -> Self {
        let svc_stash = svc_stash();
        assert!(!svc_stash.is_invalid());
        let stashed_svc = single_stashed_svc(svc_stash);
        let local_svc = take_svc_channel();
        let test = Self { local_svc, stashed_svc };
        test.drain_stashed_svc();
        test
    }

    /// Drains any preexisting messages from `stashed_svc` so tests can observe
    /// state transitions cleanly even in instrumented builds that publish debug
    /// data before `main`.
    fn drain_stashed_svc(&self) {
        on_each_message(&self.stashed_svc, |stashed_svc| {
            let mut bytes = Vec::new();
            let mut handles = Vec::new();
            stashed_svc
                .read_split(&mut bytes, &mut handles)
                .expect("failed to drain message from stashed_svc");
            true
        });
    }

    fn svc_stub_is_valid_handle(&self) {
        self.drain_stashed_svc();
        assert!(!self.local_svc.is_invalid());
    }

    fn writing_into_svc_shows_up_in_stash_handle(&self) {
        self.drain_stashed_svc();
        assert!(!self.local_svc.is_invalid());
        assert!(!self.stashed_svc.is_invalid());

        let written_message = b"Hello World!\0";
        self.local_svc.write(written_message, &mut []).expect("failed to write to local_svc");

        let mut read_message = Vec::new();
        let mut read_handles = Vec::new();
        self.stashed_svc
            .read_split(&mut read_message, &mut read_handles)
            .expect("failed to read from stashed_svc");
        assert_eq!(read_message.as_slice(), written_message);
        assert!(read_handles.is_empty());
    }

    fn sanitizer_publish_data_shows_up_in_stashed_handle(&self) {
        self.drain_stashed_svc();
        assert!(!self.local_svc.is_invalid());
        assert!(!self.stashed_svc.is_invalid());

        let vmo = Vmo::create(124).expect("failed to create VMO");

        // Channel transitions from not readable to readable.
        assert!(matches!(
            self.stashed_svc.wait_one(Signals::CHANNEL_READABLE, MonotonicInstant::INFINITE_PAST),
            WaitResult::TimedOut(_)
        ));

        const DATA_SINK: &str = "some_sink_name";
        const DATA_SINK_CSTR: &CStr = c"some_sink_name";
        let vmo_koid = koid(vmo.as_handle_ref());
        let token_1: EventPair = publish_data(DATA_SINK_CSTR, vmo).into();
        assert!(!token_1.is_invalid());

        let token_koid = peer_koid(token_1.as_handle_ref());
        assert_ne!(token_koid, Koid::from_raw(ZX_KOID_INVALID));
        assert_ne!(vmo_koid, Koid::from_raw(ZX_KOID_INVALID));

        let mut found = false;
        on_each_message(&self.stashed_svc, |stashed_svc| {
            let debug_msg = debug_data_message(stashed_svc);
            if debug_msg.sink != DATA_SINK {
                return true;
            }
            assert_eq!(koid(debug_msg.vmo.as_handle_ref()), vmo_koid);
            assert_eq!(koid(debug_msg.token.as_handle_ref()), token_koid);
            found = true;
            false
        });
        assert!(found);
    }
}

fn main() {
    init_test_logging();
    let test = SvcSingleProcessTest::new();
    test.svc_stub_is_valid_handle();
    test.writing_into_svc_shows_up_in_stash_handle();
    test.sanitizer_publish_data_shows_up_in_stashed_handle();
}
