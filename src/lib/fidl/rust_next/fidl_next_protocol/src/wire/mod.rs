// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL protocol wire types definitions and implementations.

mod epitaph;
mod flexible;
mod flexible_result;
mod framework_error;
mod message_header;

pub use self::epitaph::*;
pub use self::flexible::*;
pub use self::flexible_result::*;
pub use self::framework_error::*;
pub use self::message_header::*;
