// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use helper::{
    debug_data_message, init_test_logging, koid, on_each_message, peer_koid, stashed_svc,
    svc_stash, take_svc_channel,
};
use published_data::{PUBLISHER_SINK_NAME, VMO_CONTENTS};
use std::ffi::CStr;
use std::str::from_utf8;
use zx::sys::ZX_KOID_INVALID;
use zx::{Channel, EventPair, Koid, MonotonicInstant, Signals, Vmo, WaitResult};
use zx_libc::sanitizer::publish_data;

struct MultipleProcessSvcTest {
    local_svc: Channel,
    stashed_svc: Channel,
    provider_svc: Channel,
}

impl MultipleProcessSvcTest {
    fn new() -> Self {
        let svc_stash = svc_stash();
        assert!(!svc_stash.is_invalid());

        // Order matters: provider is stored first.
        let (provider_svc, stashed_svc) = stashed_svc(svc_stash);
        let provider_svc = provider_svc.expect("expected provider svc in stash");
        let local_svc = take_svc_channel();

        Self { local_svc, stashed_svc, provider_svc }
    }

    fn provider_data_matches_expectations(&self) {
        let mut found = false;
        on_each_message(&self.provider_svc, |stashed_svc| {
            let debug_msg = debug_data_message(stashed_svc);
            if debug_msg.sink != PUBLISHER_SINK_NAME {
                return true;
            }
            found = true;

            // Token must be signaled with peer closed.
            match debug_msg
                .token
                .wait_one(Signals::EVENTPAIR_PEER_CLOSED, MonotonicInstant::INFINITE_PAST)
            {
                WaitResult::Ok(observed) => {
                    assert!(observed.contains(Signals::EVENTPAIR_PEER_CLOSED));
                }
                other => {
                    panic!(
                        "expected token to be signaled with EVENTPAIR_PEER_CLOSED, got: {other:?}"
                    );
                }
            }

            // Check VMO contents.
            let mut actual_contents = [0u8; VMO_CONTENTS.len()];
            debug_msg.vmo.read(&mut actual_contents, 0).expect("failed to read VMO contents");
            assert_eq!(from_utf8(&actual_contents).expect("valid utf-8"), VMO_CONTENTS);
            false
        });
        assert!(found);
    }

    fn sanitizer_publish_data_shows_up_in_stashed_handle(&self) {
        assert!(!self.local_svc.is_invalid());
        assert!(!self.stashed_svc.is_invalid());

        let vmo = Vmo::create(124).expect("failed to create VMO");

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
            found = true;

            assert_eq!(debug_msg.sink, DATA_SINK);
            assert_eq!(koid(debug_msg.vmo.as_handle_ref()), vmo_koid);
            assert_eq!(koid(debug_msg.token.as_handle_ref()), token_koid);
            false
        });
        assert!(found);
    }
}

fn main() {
    init_test_logging();
    let test = MultipleProcessSvcTest::new();
    test.provider_data_matches_expectations();
    test.sanitizer_publish_data_shows_up_in_stashed_handle();
}
