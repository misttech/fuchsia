// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    #[link_name = "supports_fixed_range"]
    pub(super) static mut SUPPORTS_FIXED_RANGE: bool;

    #[link_name = "num_variable"]
    pub(super) static mut NUM_VARIABLE: u8;

    #[link_name = "g_max_paddr_width"]
    pub(super) static mut G_MAX_PADDR_WIDTH: u8;
}
