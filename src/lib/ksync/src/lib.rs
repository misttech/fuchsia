// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

extern crate self as ksync;

pub use ksync_macro::guarded;

mod kcell;
mod kmutex;
mod lock_token;

pub use kcell::KCell;
pub use kmutex::{KMutex, KMutexGuard};
pub use lock_token::LockToken;
