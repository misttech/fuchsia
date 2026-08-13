// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// KVM MSRs

/// Enable paravirtual fast APIC EOI
pub const X86_MSR_KVM_PV_EOI_EN: u32 = 0x4b56_4d04;
pub const X86_MSR_KVM_PV_EOI_EN_ENABLE: u64 = 1 << 0;
