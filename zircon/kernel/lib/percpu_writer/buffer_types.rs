// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Rust bindings and methods for `percpu_writer` dropped-record tracking structures.

#![no_std]

use zx_types::zx_instant_boot_ticks_t;

mod sys {
    include!(rustenv_path::envpath!("PERCPU_WRITER_BINDINGS_RS"));
}

pub use sys::{
    percpu_writer_DroppedRecordDurationEvent as DroppedRecordDurationEvent,
    percpu_writer_DroppedRecordStats as DroppedRecordStats,
};

impl Default for DroppedRecordStats {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl DroppedRecordStats {
    /// Creates a zero-initialized `DroppedRecordStats`.
    #[inline]
    pub const fn new() -> Self {
        Self {
            first_dropped: 0,
            last_dropped: 0,
            num_dropped: 0,
            bytes_dropped: 0,
            has_dropped: false,
            __bindgen_padding_0: [0; 7],
        }
    }

    /// Resets the dropped record statistics to their initial values.
    #[inline]
    pub fn reset(&mut self) {
        self.first_dropped = 0;
        self.last_dropped = 0;
        self.num_dropped = 0;
        self.bytes_dropped = 0;
        self.has_dropped = false;
    }

    /// Records a dropped trace record of `size` bytes at timestamp `now`.
    #[inline]
    pub fn track(&mut self, now: zx_instant_boot_ticks_t, size: u32) {
        if !self.has_dropped {
            self.first_dropped = now;
            self.has_dropped = true;
        }
        self.last_dropped = now;
        self.num_dropped = self.num_dropped.wrapping_add(1);
        self.bytes_dropped = self.bytes_dropped.wrapping_add(size);
    }

    /// Returns true if any records have been dropped since the last reset.
    #[inline]
    pub const fn has_dropped(&self) -> bool {
        self.has_dropped
    }
}
