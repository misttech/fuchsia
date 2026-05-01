// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

use zr as _;

mod canary;
mod ring_buffer;

pub use canary::{Canary, magic};
pub use ring_buffer::RingBuffer;
