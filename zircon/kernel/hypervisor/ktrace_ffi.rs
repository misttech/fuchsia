// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::ktrace::{VcpuBlockOp, VcpuMeta};

unsafe extern "C" {
    pub(super) fn cpp_ktrace_vcpu(block_op: VcpuBlockOp, meta: VcpuMeta);
}
