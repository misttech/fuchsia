// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::traits;

pub struct MagmaSystemSemaphore {
    global_id: u64,
    msd_semaphore: Box<dyn traits::Semaphore>,
}

impl MagmaSystemSemaphore {
    pub fn new(global_id: u64, msd_semaphore: Box<dyn traits::Semaphore>) -> Self {
        Self { global_id, msd_semaphore }
    }

    pub fn global_id(&self) -> u64 {
        self.global_id
    }

    pub fn msd_semaphore(&self) -> &dyn traits::Semaphore {
        &*self.msd_semaphore
    }
}
