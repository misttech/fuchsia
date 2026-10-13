// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::game::GameCommand;
use fidl_fuchsia_math::SizeU;
use fidl_fuchsia_ui_composition as fland;

#[derive(Debug, PartialEq)]
pub(crate) enum InternalMessage {
    OnPresentError { error: fland::FlatlandError },
    OnNextFrameBegin,
    Relayout { size: SizeU },
    Tick,
    Command(GameCommand),
    PointerTap { x: f32, y: f32 },
    ViewDismissed,
}
