// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

use crate::error;
use crate::errors::Errno;
use inspect_stubs::track_stub;

#[derive(Clone, Copy, PartialEq)]
#[repr(usize)]
pub enum ElfNoteType {
    // NT_PRSTATUS
    PrStatus = 1,
    // NT_FPREGSET
    FpRegSet = 2,
    // NT_X86_XSTATE
    X86_XState = 0x202,
    // NT_ARM_TAGGED_ADDR_CTRL
    ArmTaggedAddrCtrl = 0x409,
    // NT_ARM_PAC_ENABLED_KEYS
    ArmPacEnabledKeys = 0x40a,
}

impl TryFrom<usize> for ElfNoteType {
    type Error = Errno;

    fn try_from(v: usize) -> Result<Self, Errno> {
        match v {
            x if x == ElfNoteType::PrStatus as usize => Ok(ElfNoteType::PrStatus),
            x if x == ElfNoteType::FpRegSet as usize => Ok(ElfNoteType::FpRegSet),
            x if x == ElfNoteType::X86_XState as usize => Ok(ElfNoteType::X86_XState),
            x if x == ElfNoteType::ArmTaggedAddrCtrl as usize => {
                track_stub!(TODO("https://fxbug.dev/441149562"), "NT_ARM_TAGGED_ADDR_CTRL");
                error!(ENOTSUP)
            }
            x if x == ElfNoteType::ArmPacEnabledKeys as usize => {
                track_stub!(TODO("https://fxbug.dev/441149562"), "NT_ARM_PAC_ENABLED_KEYS");
                error!(ENOTSUP)
            }
            _ => error!(EINVAL),
        }
    }
}
