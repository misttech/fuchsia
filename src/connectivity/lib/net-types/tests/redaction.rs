// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Verifies that `net_types::ethernet::Mac` is wired up to the process-global
//! redactor.
//!
//! This lives in its own test binary because installing a global redactor
//! affects every `Mac` formatted in the process for the rest of its lifetime.

use net_types::ethernet::Mac;

#[test]
fn mac_display_and_debug_follow_global_redactor() {
    let mac = Mac::new([0x00, 0x1a, 0xaa, 0x12, 0x34, 0x56]);

    // Before any redactor is installed the raw address is printed.
    assert_eq!(format!("{mac}"), "00:1a:aa:12:34:56");
    assert_eq!(format!("{mac:?}"), "00:1a:aa:12:34:56");

    // After redaction is enabled, both `{}` and `{:?}` are redacted (OUI
    // preserved, remaining octets obscured), rendered in the same shape plus
    // the shared marker.
    redactor::set_masking_redactor();
    assert_eq!(format!("{mac}"), "00:1a:aa:00:00:00*");
    assert_eq!(format!("{mac:?}"), "00:1a:aa:00:00:00*");
}
