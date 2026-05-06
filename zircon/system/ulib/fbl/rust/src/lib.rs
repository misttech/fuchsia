// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]

mod array;
mod canary;
mod conditional_select_nospec;
mod confine_array_index;
mod inline_array;
mod ring_buffer;
mod vector;

pub use array::Array;
pub use canary::{Canary, magic};
pub use conditional_select_nospec::{conditional_select_nospec_eq, conditional_select_nospec_lt};
pub use confine_array_index::confine_array_index;
pub use inline_array::InlineArray;
pub use ring_buffer::RingBuffer;
pub use vector::Vector;
