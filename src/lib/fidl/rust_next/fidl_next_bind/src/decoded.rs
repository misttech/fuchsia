// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_codec::{Decoded, IntoNatural};
use fidl_next_protocol::Transport;

use crate::HasTransport;

use super::Method;

/// A decoded request.
pub type Request<M, T = <<M as Method>::Protocol as HasTransport>::Transport> =
    Decoded<<M as Method>::Request, <T as Transport>::RecvBuffer>;

/// The wire type for a decoded response.
pub type WireResponse<M, T = <<M as Method>::Protocol as HasTransport>::Transport> =
    Decoded<<M as Method>::Response, <T as Transport>::RecvBuffer>;

/// A decoded response.
pub type NaturalResponse<M> = <<M as Method>::Response as IntoNatural>::Natural;
