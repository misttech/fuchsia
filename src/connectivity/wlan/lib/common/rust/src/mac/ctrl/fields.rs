// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ieee80211::{Bssid, MacAddr};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

// IEEE Std 802.11-2016, 9.3.1.5
#[derive(
    KnownLayout, FromBytes, IntoBytes, Immutable, Unaligned, PartialEq, Eq, Clone, Copy, Debug,
)]
#[repr(C, packed)]
pub struct PsPoll {
    pub masked_aid: u16,
    pub bssid: Bssid,
    pub ta: MacAddr,
}
