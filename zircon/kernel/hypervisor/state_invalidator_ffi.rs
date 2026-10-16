// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::state_invalidator::StateInvalidator;

#[allow(improper_ctypes)]
unsafe extern "C" {
    pub(super) fn cpp_state_invalidator_invalidate(invalidator: *mut StateInvalidator);
}
