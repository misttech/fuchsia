// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

.macro zircon_syscall num, name, caller
    li t0, \num
    ecall
// This symbol at the return address identifies this as an approved call site.
    .hidden CODE_SYSRET_\name\()_VIA_\caller
CODE_SYSRET_\name\()_VIA_\caller\():
.endm
