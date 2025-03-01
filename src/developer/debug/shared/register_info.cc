// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/shared/register_info.h"

#include <lib/syslog/cpp/macros.h>

#include <algorithm>
#include <iterator>
#include <map>

namespace debug {
namespace {

// clang-format off

// Canonical registers, these all have a 1:1 mapping between "id" and "name".
const RegisterInfo kRegisterInfo[] = {
    // ARMv8
    // ---------------------------------------------------------------------------------------------

    // General purpose.

    {.id = RegisterID::kARMv8_x0,  .name = "x0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x0,  .bits = 64, .dwarf_id = 0},
    {.id = RegisterID::kARMv8_x1,  .name = "x1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x1,  .bits = 64, .dwarf_id = 1},
    {.id = RegisterID::kARMv8_x2,  .name = "x2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x2,  .bits = 64, .dwarf_id = 2},
    {.id = RegisterID::kARMv8_x3,  .name = "x3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x3,  .bits = 64, .dwarf_id = 3},
    {.id = RegisterID::kARMv8_x4,  .name = "x4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x4,  .bits = 64, .dwarf_id = 4},
    {.id = RegisterID::kARMv8_x5,  .name = "x5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x5,  .bits = 64, .dwarf_id = 5},
    {.id = RegisterID::kARMv8_x6,  .name = "x6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x6,  .bits = 64, .dwarf_id = 6},
    {.id = RegisterID::kARMv8_x7,  .name = "x7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x7,  .bits = 64, .dwarf_id = 7},
    {.id = RegisterID::kARMv8_x8,  .name = "x8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x8,  .bits = 64, .dwarf_id = 8},
    {.id = RegisterID::kARMv8_x9,  .name = "x9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x9,  .bits = 64, .dwarf_id = 9},
    {.id = RegisterID::kARMv8_x10, .name = "x10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x10, .bits = 64, .dwarf_id = 10},
    {.id = RegisterID::kARMv8_x11, .name = "x11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x11, .bits = 64, .dwarf_id = 11},
    {.id = RegisterID::kARMv8_x12, .name = "x12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x12, .bits = 64, .dwarf_id = 12},
    {.id = RegisterID::kARMv8_x13, .name = "x13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x13, .bits = 64, .dwarf_id = 13},
    {.id = RegisterID::kARMv8_x14, .name = "x14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x14, .bits = 64, .dwarf_id = 14},
    {.id = RegisterID::kARMv8_x15, .name = "x15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x15, .bits = 64, .dwarf_id = 15},
    {.id = RegisterID::kARMv8_x16, .name = "x16", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x16, .bits = 64, .dwarf_id = 16},
    {.id = RegisterID::kARMv8_x17, .name = "x17", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x17, .bits = 64, .dwarf_id = 17},
    {.id = RegisterID::kARMv8_x18, .name = "x18", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x18, .bits = 64, .dwarf_id = 18},
    {.id = RegisterID::kARMv8_x19, .name = "x19", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x19, .bits = 64, .dwarf_id = 19},
    {.id = RegisterID::kARMv8_x20, .name = "x20", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x20, .bits = 64, .dwarf_id = 20},
    {.id = RegisterID::kARMv8_x21, .name = "x21", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x21, .bits = 64, .dwarf_id = 21},
    {.id = RegisterID::kARMv8_x22, .name = "x22", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x22, .bits = 64, .dwarf_id = 22},
    {.id = RegisterID::kARMv8_x23, .name = "x23", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x23, .bits = 64, .dwarf_id = 23},
    {.id = RegisterID::kARMv8_x24, .name = "x24", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x24, .bits = 64, .dwarf_id = 24},
    {.id = RegisterID::kARMv8_x25, .name = "x25", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x25, .bits = 64, .dwarf_id = 25},
    {.id = RegisterID::kARMv8_x26, .name = "x26", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x26, .bits = 64, .dwarf_id = 26},
    {.id = RegisterID::kARMv8_x27, .name = "x27", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x27, .bits = 64, .dwarf_id = 27},
    {.id = RegisterID::kARMv8_x28, .name = "x28", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x28, .bits = 64, .dwarf_id = 28},
    {.id = RegisterID::kARMv8_x29, .name = "x29", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x29, .bits = 64, .dwarf_id = 29},
    {.id = RegisterID::kARMv8_lr,  .name = "lr",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_lr,  .bits = 64, .dwarf_id = 30, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_sp,  .name = "sp",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_sp,  .bits = 64, .dwarf_id = 31, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_pc,  .name = "pc",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_pc,  .bits = 64, .dwarf_id = 32, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_tpidr, .name = "tpidr", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_tpidr, .bits = 64, .dwarf_id = 36},

    {.id = RegisterID::kARMv8_cpsr, .name = "cpsr", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_cpsr, .bits = 64, .format = RegisterFormat::kSpecial},

    // FP (none defined for ARM64).

    // Vector.

    {.id = RegisterID::kARMv8_fpcr, .name = "fpcr", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_fpcr, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_fpsr, .name = "fpsr", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_fpsr, .bits = 32, .format = RegisterFormat::kSpecial},

    {.id = RegisterID::kARMv8_v0,  .name = "v0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v0,  .bits = 128, .dwarf_id = 64, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v1,  .name = "v1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v1,  .bits = 128, .dwarf_id = 65, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v2,  .name = "v2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v2,  .bits = 128, .dwarf_id = 66, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v3,  .name = "v3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v3,  .bits = 128, .dwarf_id = 67, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v4,  .name = "v4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v4,  .bits = 128, .dwarf_id = 68, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v5,  .name = "v5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v5,  .bits = 128, .dwarf_id = 69, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v6,  .name = "v6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v6,  .bits = 128, .dwarf_id = 70, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v7,  .name = "v7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v7,  .bits = 128, .dwarf_id = 71, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v8,  .name = "v8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v8,  .bits = 128, .dwarf_id = 72, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v9,  .name = "v9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v9,  .bits = 128, .dwarf_id = 73, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v10, .name = "v10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v10, .bits = 128, .dwarf_id = 74, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v11, .name = "v11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v11, .bits = 128, .dwarf_id = 75, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v12, .name = "v12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v12, .bits = 128, .dwarf_id = 76, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v13, .name = "v13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v13, .bits = 128, .dwarf_id = 77, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v14, .name = "v14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v14, .bits = 128, .dwarf_id = 78, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v15, .name = "v15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v15, .bits = 128, .dwarf_id = 79, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v16, .name = "v16", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v16, .bits = 128, .dwarf_id = 80, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v17, .name = "v17", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v17, .bits = 128, .dwarf_id = 81, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v18, .name = "v18", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v18, .bits = 128, .dwarf_id = 82, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v19, .name = "v19", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v19, .bits = 128, .dwarf_id = 83, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v20, .name = "v20", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v20, .bits = 128, .dwarf_id = 84, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v21, .name = "v21", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v21, .bits = 128, .dwarf_id = 85, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v22, .name = "v22", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v22, .bits = 128, .dwarf_id = 86, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v23, .name = "v23", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v23, .bits = 128, .dwarf_id = 87, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v24, .name = "v24", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v24, .bits = 128, .dwarf_id = 88, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v25, .name = "v25", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v25, .bits = 128, .dwarf_id = 89, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v26, .name = "v26", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v26, .bits = 128, .dwarf_id = 90, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v27, .name = "v27", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v27, .bits = 128, .dwarf_id = 91, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v28, .name = "v28", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v28, .bits = 128, .dwarf_id = 92, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v29, .name = "v29", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v29, .bits = 128, .dwarf_id = 93, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v30, .name = "v30", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v30, .bits = 128, .dwarf_id = 94, .format = RegisterFormat::kVector},
    {.id = RegisterID::kARMv8_v31, .name = "v31", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v31, .bits = 128, .dwarf_id = 95, .format = RegisterFormat::kVector},

    // Debug.

    {.id = RegisterID::kARMv8_id_aa64dfr0_el1, .name = "id_aa64dfr0", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_id_aa64dfr0_el1, .bits = 64, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_mdscr_el1,       .name = "mdscr",       .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_mdscr_el1,       .bits = 64, .format = RegisterFormat::kSpecial},

    // Hardware breakpoint control registers.
    {.id = RegisterID::kARMv8_dbgbcr0_el1,  .name = "dbgbcr0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr0_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr1_el1,  .name = "dbgbcr1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr1_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr2_el1,  .name = "dbgbcr2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr2_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr3_el1,  .name = "dbgbcr3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr3_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr4_el1,  .name = "dbgbcr4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr4_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr5_el1,  .name = "dbgbcr5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr5_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr6_el1,  .name = "dbgbcr6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr6_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr7_el1,  .name = "dbgbcr7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr7_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr8_el1,  .name = "dbgbcr8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr8_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr9_el1,  .name = "dbgbcr9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr9_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr10_el1, .name = "dbgbcr10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr10_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr11_el1, .name = "dbgbcr11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr11_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr12_el1, .name = "dbgbcr12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr12_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr13_el1, .name = "dbgbcr13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr13_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr14_el1, .name = "dbgbcr14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr14_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgbcr15_el1, .name = "dbgbcr15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbcr15_el1, .bits = 32, .format = RegisterFormat::kSpecial},

    // Hardware breakpoint value (address) registers.
    {.id = RegisterID::kARMv8_dbgbvr0_el1,  .name = "dbgbvr0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr0_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr1_el1,  .name = "dbgbvr1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr1_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr2_el1,  .name = "dbgbvr2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr2_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr3_el1,  .name = "dbgbvr3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr3_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr4_el1,  .name = "dbgbvr4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr4_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr5_el1,  .name = "dbgbvr5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr5_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr6_el1,  .name = "dbgbvr6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr6_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr7_el1,  .name = "dbgbvr7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr7_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr8_el1,  .name = "dbgbvr8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr8_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr9_el1,  .name = "dbgbvr9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr9_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr10_el1, .name = "dbgbvr10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr10_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr11_el1, .name = "dbgbvr11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr11_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr12_el1, .name = "dbgbvr12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr12_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr13_el1, .name = "dbgbvr13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr13_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr14_el1, .name = "dbgbvr14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr14_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgbvr15_el1, .name = "dbgbvr15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgbvr15_el1, .bits = 64, .format = RegisterFormat::kWordAddress},

    // Watchpoint control registers.
    {.id = RegisterID::kARMv8_dbgwcr0_el1,  .name = "dbgwcr0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr0_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr1_el1,  .name = "dbgwcr1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr1_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr2_el1,  .name = "dbgwcr2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr2_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr3_el1,  .name = "dbgwcr3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr3_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr4_el1,  .name = "dbgwcr4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr4_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr5_el1,  .name = "dbgwcr5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr5_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr6_el1,  .name = "dbgwcr6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr6_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr7_el1,  .name = "dbgwcr7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr7_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr8_el1,  .name = "dbgwcr8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr8_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr9_el1,  .name = "dbgwcr9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr9_el1,  .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr10_el1, .name = "dbgwcr10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr10_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr11_el1, .name = "dbgwcr11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr11_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr12_el1, .name = "dbgwcr12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr12_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr13_el1, .name = "dbgwcr13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr13_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr14_el1, .name = "dbgwcr14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr14_el1, .bits = 32, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kARMv8_dbgwcr15_el1, .name = "dbgwcr15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwcr15_el1, .bits = 32, .format = RegisterFormat::kSpecial},

    // Watchpoint value (address) registers.
    {.id = RegisterID::kARMv8_dbgwvr0_el1,  .name = "dbgwvr0",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr0_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr1_el1,  .name = "dbgwvr1",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr1_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr2_el1,  .name = "dbgwvr2",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr2_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr3_el1,  .name = "dbgwvr3",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr3_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr4_el1,  .name = "dbgwvr4",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr4_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr5_el1,  .name = "dbgwvr5",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr5_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr6_el1,  .name = "dbgwvr6",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr6_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr7_el1,  .name = "dbgwvr7",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr7_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr8_el1,  .name = "dbgwvr8",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr8_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr9_el1,  .name = "dbgwvr9",  .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr9_el1,  .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr10_el1, .name = "dbgwvr10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr10_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr11_el1, .name = "dbgwvr11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr11_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr12_el1, .name = "dbgwvr12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr12_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr13_el1, .name = "dbgwvr13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr13_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr14_el1, .name = "dbgwvr14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr14_el1, .bits = 64, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kARMv8_dbgwvr15_el1, .name = "dbgwvr15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_dbgwvr15_el1, .bits = 64, .format = RegisterFormat::kWordAddress},

    // General-purpose aliases.

    // Our canonical name for x30 is "LR".
    {.id = RegisterID::kARMv8_x30, .name = "x30", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_lr, .bits = 64, .format = RegisterFormat::kWordAddress},

    // Aliases for the low 32-bit registers.
    {.id = RegisterID::kARMv8_w0, .name = "w0", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x0, .bits = 32},
    {.id = RegisterID::kARMv8_w1, .name = "w1", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x1, .bits = 32},
    {.id = RegisterID::kARMv8_w2, .name = "w2", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x2, .bits = 32},
    {.id = RegisterID::kARMv8_w3, .name = "w3", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x3, .bits = 32},
    {.id = RegisterID::kARMv8_w4, .name = "w4", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x4, .bits = 32},
    {.id = RegisterID::kARMv8_w5, .name = "w5", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x5, .bits = 32},
    {.id = RegisterID::kARMv8_w6, .name = "w6", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x6, .bits = 32},
    {.id = RegisterID::kARMv8_w7, .name = "w7", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x7, .bits = 32},
    {.id = RegisterID::kARMv8_w8, .name = "w8", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x8, .bits = 32},
    {.id = RegisterID::kARMv8_w9, .name = "w9", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x9, .bits = 32},
    {.id = RegisterID::kARMv8_w10, .name = "w10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x10, .bits = 32},
    {.id = RegisterID::kARMv8_w11, .name = "w11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x11, .bits = 32},
    {.id = RegisterID::kARMv8_w12, .name = "w12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x12, .bits = 32},
    {.id = RegisterID::kARMv8_w13, .name = "w13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x13, .bits = 32},
    {.id = RegisterID::kARMv8_w14, .name = "w14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x14, .bits = 32},
    {.id = RegisterID::kARMv8_w15, .name = "w15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x15, .bits = 32},
    {.id = RegisterID::kARMv8_w16, .name = "w16", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x16, .bits = 32},
    {.id = RegisterID::kARMv8_w17, .name = "w17", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x17, .bits = 32},
    {.id = RegisterID::kARMv8_w18, .name = "w18", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x18, .bits = 32},
    {.id = RegisterID::kARMv8_w19, .name = "w19", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x19, .bits = 32},
    {.id = RegisterID::kARMv8_w20, .name = "w20", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x20, .bits = 32},
    {.id = RegisterID::kARMv8_w21, .name = "w21", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x21, .bits = 32},
    {.id = RegisterID::kARMv8_w22, .name = "w22", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x22, .bits = 32},
    {.id = RegisterID::kARMv8_w23, .name = "w23", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x23, .bits = 32},
    {.id = RegisterID::kARMv8_w24, .name = "w24", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x24, .bits = 32},
    {.id = RegisterID::kARMv8_w25, .name = "w25", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x25, .bits = 32},
    {.id = RegisterID::kARMv8_w26, .name = "w26", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x26, .bits = 32},
    {.id = RegisterID::kARMv8_w27, .name = "w27", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x27, .bits = 32},
    {.id = RegisterID::kARMv8_w28, .name = "w28", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x28, .bits = 32},
    {.id = RegisterID::kARMv8_w29, .name = "w29", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x29, .bits = 32},
    {.id = RegisterID::kARMv8_w30, .name = "w30", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_x30, .bits = 32},

    // Double-precision floating point (low 64 bits of the vector registers).
    {.id = RegisterID::kARMv8_d0, .name = "d0", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v0, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d1, .name = "d1", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v1, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d2, .name = "d2", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v2, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d3, .name = "d3", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v3, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d4, .name = "d4", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v4, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d5, .name = "d5", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v5, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d6, .name = "d6", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v6, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d7, .name = "d7", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v7, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d8, .name = "d8", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v8, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d9, .name = "d9", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v9, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d10, .name = "d10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v10, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d11, .name = "d11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v11, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d12, .name = "d12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v12, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d13, .name = "d13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v13, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d14, .name = "d14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v14, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d15, .name = "d15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v15, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d16, .name = "d16", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v16, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d17, .name = "d17", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v17, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d18, .name = "d18", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v18, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d19, .name = "d19", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v19, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d20, .name = "d20", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v20, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d21, .name = "d21", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v21, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d22, .name = "d22", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v22, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d23, .name = "d23", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v23, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d24, .name = "d24", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v24, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d25, .name = "d25", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v25, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d26, .name = "d26", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v26, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d27, .name = "d27", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v27, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d28, .name = "d28", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v28, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d29, .name = "d29", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v29, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d30, .name = "d30", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v30, .bits = 64, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_d31, .name = "d31", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v31, .bits = 64, .format = RegisterFormat::kFloat},

    // Single-precision floating point (low 32 bits of the vector registers).
    {.id = RegisterID::kARMv8_s0, .name = "s0", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v0, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s1, .name = "s1", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v1, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s2, .name = "s2", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v2, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s3, .name = "s3", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v3, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s4, .name = "s4", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v4, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s5, .name = "s5", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v5, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s6, .name = "s6", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v6, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s7, .name = "s7", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v7, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s8, .name = "s8", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v8, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s9, .name = "s9", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v9, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s10, .name = "s10", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v10, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s11, .name = "s11", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v11, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s12, .name = "s12", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v12, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s13, .name = "s13", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v13, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s14, .name = "s14", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v14, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s15, .name = "s15", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v15, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s16, .name = "s16", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v16, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s17, .name = "s17", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v17, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s18, .name = "s18", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v18, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s19, .name = "s19", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v19, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s20, .name = "s20", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v20, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s21, .name = "s21", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v21, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s22, .name = "s22", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v22, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s23, .name = "s23", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v23, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s24, .name = "s24", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v24, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s25, .name = "s25", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v25, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s26, .name = "s26", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v26, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s27, .name = "s27", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v27, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s28, .name = "s28", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v28, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s29, .name = "s29", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v29, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s30, .name = "s30", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v30, .bits = 32, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kARMv8_s31, .name = "s31", .arch = Arch::kArm64, .canonical_id = RegisterID::kARMv8_v31, .bits = 32, .format = RegisterFormat::kFloat},

    // x64
    // ---------------------------------------------------------------------------------------------

    // General purpose.

    {.id = RegisterID::kX64_rax, .name = "rax", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rax, .bits = 64, .dwarf_id = 0},
    {.id = RegisterID::kX64_rbx, .name = "rbx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbx, .bits = 64, .dwarf_id = 3},
    {.id = RegisterID::kX64_rcx, .name = "rcx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rcx, .bits = 64, .dwarf_id = 2},
    {.id = RegisterID::kX64_rdx, .name = "rdx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdx, .bits = 64, .dwarf_id = 1},
    {.id = RegisterID::kX64_rsi, .name = "rsi", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rsi, .bits = 64, .dwarf_id = 4},
    {.id = RegisterID::kX64_rdi, .name = "rdi", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdi, .bits = 64, .dwarf_id = 5},
    {.id = RegisterID::kX64_rbp, .name = "rbp", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbp, .bits = 64, .dwarf_id = 6, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kX64_rsp, .name = "rsp", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rsp, .bits = 64, .dwarf_id = 7, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kX64_r8,  .name = "r8",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r8,  .bits = 64, .dwarf_id = 8},
    {.id = RegisterID::kX64_r9,  .name = "r9",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r9,  .bits = 64, .dwarf_id = 9},
    {.id = RegisterID::kX64_r10, .name = "r10", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r10, .bits = 64, .dwarf_id = 10},
    {.id = RegisterID::kX64_r11, .name = "r11", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r11, .bits = 64, .dwarf_id = 11},
    {.id = RegisterID::kX64_r12, .name = "r12", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r12, .bits = 64, .dwarf_id = 12},
    {.id = RegisterID::kX64_r13, .name = "r13", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r13, .bits = 64, .dwarf_id = 13},
    {.id = RegisterID::kX64_r14, .name = "r14", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r14, .bits = 64, .dwarf_id = 14},
    {.id = RegisterID::kX64_r15, .name = "r15", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_r15, .bits = 64, .dwarf_id = 15},
    {.id = RegisterID::kX64_rip, .name = "rip", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rip, .bits = 64, .dwarf_id = 16, .format = RegisterFormat::kVoidAddress},

    {.id = RegisterID::kX64_rflags, .name = "rflags", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rflags, .bits = 64, .dwarf_id = 49, .format = RegisterFormat::kSpecial},
    // See "DWARF notes" below on these weird segment registers.
    {.id = RegisterID::kX64_fsbase, .name = "fs_base", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fsbase, .bits = 64, .dwarf_id = 58, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kX64_gsbase, .name = "gs_base", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_gsbase, .bits = 64, .dwarf_id = 59, .format = RegisterFormat::kSpecial},

    // General-purpose aliases.

    {.id = RegisterID::kX64_ah,  .name = "ah",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rax, .bits = 8, .shift = 8},
    {.id = RegisterID::kX64_al,  .name = "al",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rax, .bits = 8},
    {.id = RegisterID::kX64_ax,  .name = "ax",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rax, .bits = 16},
    {.id = RegisterID::kX64_eax, .name = "eax", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rax, .bits = 32},

    {.id = RegisterID::kX64_bh,  .name = "bh",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbx, .bits = 8, .shift = 8},
    {.id = RegisterID::kX64_bl,  .name = "bl",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbx, .bits = 8},
    {.id = RegisterID::kX64_bx,  .name = "bx",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbx, .bits = 16},
    {.id = RegisterID::kX64_ebx, .name = "ebx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rbx, .bits = 32},

    {.id = RegisterID::kX64_ch,  .name = "ch",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rcx, .bits = 8, .shift = 8},
    {.id = RegisterID::kX64_cl,  .name = "cl",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rcx, .bits = 8},
    {.id = RegisterID::kX64_cx,  .name = "cx",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rcx, .bits = 16},
    {.id = RegisterID::kX64_ecx, .name = "ecx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rcx, .bits = 32},

    {.id = RegisterID::kX64_dh,  .name = "dh",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdx, .bits = 8, .shift = 8},
    {.id = RegisterID::kX64_dl,  .name = "dl",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdx, .bits = 8},
    {.id = RegisterID::kX64_dx,  .name = "dx",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdx, .bits = 16},
    {.id = RegisterID::kX64_edx, .name = "edx", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdx, .bits = 32},

    {.id = RegisterID::kX64_si,  .name = "si",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rsi, .bits = 16},
    {.id = RegisterID::kX64_esi, .name = "esi", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rsi, .bits = 32},

    {.id = RegisterID::kX64_di,  .name = "di",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdi, .bits = 16},
    {.id = RegisterID::kX64_edi, .name = "edi", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_rdi, .bits = 32},

    // Note we don't have an entry for bp/ebp, sp/esp, and ip/eip because these are all pointers
    // and the low bits are more likely to be user error (they wanted the whole thing) and we don't
    // want to be misleading in those cases.

    // FP.
    {.id = RegisterID::kX64_fcw, .name = "fcw", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fcw, .bits = 16, .dwarf_id = 65, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kX64_fsw, .name = "fsw", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fsw, .bits = 16, .dwarf_id = 66, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kX64_ftw, .name = "ftw", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_ftw, .bits = 16, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kX64_fop, .name = "fop", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fop, .bits = 16, .format = RegisterFormat::kSpecial},  // 11 valid bits
    {.id = RegisterID::kX64_fip, .name = "fip", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fip, .bits = 64, .format = RegisterFormat::kVoidAddress},
    {.id = RegisterID::kX64_fdp, .name = "fdp", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_fdp, .bits = 64, .format = RegisterFormat::kVoidAddress},

    {.id = RegisterID::kX64_st0, .name = "st0", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st0, .bits = 80, .dwarf_id = 33, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st1, .name = "st1", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st1, .bits = 80, .dwarf_id = 34, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st2, .name = "st2", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st2, .bits = 80, .dwarf_id = 35, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st3, .name = "st3", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st3, .bits = 80, .dwarf_id = 36, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st4, .name = "st4", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st4, .bits = 80, .dwarf_id = 37, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st5, .name = "st5", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st5, .bits = 80, .dwarf_id = 38, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st6, .name = "st6", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st6, .bits = 80, .dwarf_id = 39, .format = RegisterFormat::kFloat},
    {.id = RegisterID::kX64_st7, .name = "st7", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st7, .bits = 80, .dwarf_id = 40, .format = RegisterFormat::kFloat},

    // Vector.

    {.id = RegisterID::kX64_mxcsr, .name = "mxcsr", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_mxcsr, .bits = 32, .dwarf_id = 64, .format = RegisterFormat::kSpecial},

    // AVX-512 (our canonical vector register names).
    {.id = RegisterID::kX64_zmm0,  .name = "zmm0",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm0,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm1,  .name = "zmm1",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm1,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm2,  .name = "zmm2",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm2,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm3,  .name = "zmm3",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm3,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm4,  .name = "zmm4",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm4,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm5,  .name = "zmm5",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm5,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm6,  .name = "zmm6",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm6,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm7,  .name = "zmm7",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm7,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm8,  .name = "zmm8",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm8,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm9,  .name = "zmm9",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm9,  .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm10, .name = "zmm10", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm10, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm11, .name = "zmm11", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm11, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm12, .name = "zmm12", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm12, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm13, .name = "zmm13", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm13, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm14, .name = "zmm14", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm14, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm15, .name = "zmm15", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm15, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm16, .name = "zmm16", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm16, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm17, .name = "zmm17", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm17, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm18, .name = "zmm18", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm18, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm19, .name = "zmm19", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm19, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm20, .name = "zmm20", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm20, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm21, .name = "zmm21", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm21, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm22, .name = "zmm22", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm22, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm23, .name = "zmm23", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm23, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm24, .name = "zmm24", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm24, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm25, .name = "zmm25", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm25, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm26, .name = "zmm26", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm26, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm27, .name = "zmm27", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm27, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm28, .name = "zmm28", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm28, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm29, .name = "zmm29", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm29, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm30, .name = "zmm30", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm30, .bits = 512, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_zmm31, .name = "zmm31", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm31, .bits = 512, .format = RegisterFormat::kVector},

    // Vector aliases

    {.id = RegisterID::kX64_xmm0,  .name = "xmm0", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm0,  .bits = 128, .dwarf_id = 17, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm1,  .name = "xmm1", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm1,  .bits = 128, .dwarf_id = 18, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm2,  .name = "xmm2", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm2,  .bits = 128, .dwarf_id = 19, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm3,  .name = "xmm3", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm3,  .bits = 128, .dwarf_id = 20, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm4,  .name = "xmm4", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm4,  .bits = 128, .dwarf_id = 21, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm5,  .name = "xmm5", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm5,  .bits = 128, .dwarf_id = 22, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm6,  .name = "xmm6", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm6,  .bits = 128, .dwarf_id = 23, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm7,  .name = "xmm7", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm7,  .bits = 128, .dwarf_id = 24, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm8,  .name = "xmm8", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm8,  .bits = 128, .dwarf_id = 25, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm9,  .name = "xmm9", .arch = Arch::kX64,  .canonical_id = RegisterID::kX64_zmm9,  .bits = 128, .dwarf_id = 26, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm10, .name = "xmm10", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm10, .bits = 128, .dwarf_id = 27, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm11, .name = "xmm11", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm11, .bits = 128, .dwarf_id = 28, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm12, .name = "xmm12", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm12, .bits = 128, .dwarf_id = 29, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm13, .name = "xmm13", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm13, .bits = 128, .dwarf_id = 30, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm14, .name = "xmm14", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm14, .bits = 128, .dwarf_id = 31, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm15, .name = "xmm15", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm15, .bits = 128, .dwarf_id = 32, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm16, .name = "xmm16", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm16, .bits = 128, .dwarf_id = 67, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm17, .name = "xmm17", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm17, .bits = 128, .dwarf_id = 68, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm18, .name = "xmm18", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm18, .bits = 128, .dwarf_id = 69, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm19, .name = "xmm19", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm19, .bits = 128, .dwarf_id = 70, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm20, .name = "xmm20", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm20, .bits = 128, .dwarf_id = 71, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm21, .name = "xmm21", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm21, .bits = 128, .dwarf_id = 72, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm22, .name = "xmm22", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm22, .bits = 128, .dwarf_id = 73, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm23, .name = "xmm23", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm23, .bits = 128, .dwarf_id = 74, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm24, .name = "xmm24", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm24, .bits = 128, .dwarf_id = 75, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm25, .name = "xmm25", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm25, .bits = 128, .dwarf_id = 76, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm26, .name = "xmm26", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm26, .bits = 128, .dwarf_id = 77, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm27, .name = "xmm27", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm27, .bits = 128, .dwarf_id = 78, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm28, .name = "xmm28", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm28, .bits = 128, .dwarf_id = 79, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm29, .name = "xmm29", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm29, .bits = 128, .dwarf_id = 80, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm30, .name = "xmm30", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm30, .bits = 128, .dwarf_id = 81, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_xmm31, .name = "xmm31", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm31, .bits = 128, .dwarf_id = 82, .format = RegisterFormat::kVector},

    {.id = RegisterID::kX64_ymm0,  .name = "ymm0",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm0,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm1,  .name = "ymm1",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm1,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm2,  .name = "ymm2",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm2,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm3,  .name = "ymm3",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm3,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm4,  .name = "ymm4",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm4,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm5,  .name = "ymm5",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm5,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm6,  .name = "ymm6",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm6,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm7,  .name = "ymm7",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm7,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm8,  .name = "ymm8",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm8,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm9,  .name = "ymm9",  .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm9,  .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm10, .name = "ymm10", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm10, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm11, .name = "ymm11", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm11, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm12, .name = "ymm12", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm12, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm13, .name = "ymm13", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm13, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm14, .name = "ymm14", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm14, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm15, .name = "ymm15", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm15, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm16, .name = "ymm16", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm16, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm17, .name = "ymm17", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm17, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm18, .name = "ymm18", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm18, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm19, .name = "ymm19", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm19, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm20, .name = "ymm20", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm20, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm21, .name = "ymm21", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm21, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm22, .name = "ymm22", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm22, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm23, .name = "ymm23", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm23, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm24, .name = "ymm24", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm24, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm25, .name = "ymm25", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm25, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm26, .name = "ymm26", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm26, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm27, .name = "ymm27", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm27, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm28, .name = "ymm28", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm28, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm29, .name = "ymm29", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm29, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm30, .name = "ymm30", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm30, .bits = 256, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_ymm31, .name = "ymm31", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_zmm31, .bits = 256, .format = RegisterFormat::kVector},

    // The old-style MMX registers are the low 64-bits of the FP registers.
    {.id = RegisterID::kX64_mm0, .name = "mm0", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st0, .bits = 64, .dwarf_id = 41, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm1, .name = "mm1", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st1, .bits = 64, .dwarf_id = 42, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm2, .name = "mm2", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st2, .bits = 64, .dwarf_id = 43, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm3, .name = "mm3", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st3, .bits = 64, .dwarf_id = 44, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm4, .name = "mm4", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st4, .bits = 64, .dwarf_id = 45, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm5, .name = "mm5", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st5, .bits = 64, .dwarf_id = 46, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm6, .name = "mm6", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st6, .bits = 64, .dwarf_id = 47, .format = RegisterFormat::kVector},
    {.id = RegisterID::kX64_mm7, .name = "mm7", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_st7, .bits = 64, .dwarf_id = 48, .format = RegisterFormat::kVector},

    // Debug.

    {.id = RegisterID::kX64_dr0, .name = "dr0", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr0, .bits = 64, .format = RegisterFormat::kVoidAddress},
    {.id = RegisterID::kX64_dr1, .name = "dr1", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr1, .bits = 64, .format = RegisterFormat::kVoidAddress},
    {.id = RegisterID::kX64_dr2, .name = "dr2", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr2, .bits = 64, .format = RegisterFormat::kVoidAddress},
    {.id = RegisterID::kX64_dr3, .name = "dr3", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr3, .bits = 64, .format = RegisterFormat::kVoidAddress},
    {.id = RegisterID::kX64_dr6, .name = "dr6", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr6, .bits = 64, .format = RegisterFormat::kSpecial},
    {.id = RegisterID::kX64_dr7, .name = "dr7", .arch = Arch::kX64, .canonical_id = RegisterID::kX64_dr7, .bits = 64, .format = RegisterFormat::kSpecial},

    // RISC-V 64
    // ---------------------------------------------------------------------------------------------

    // General purpose.

    {.id = RegisterID::kRiscv64_zero, .name = "zero", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_zero, .bits = 64, .dwarf_id = 0},
    {.id = RegisterID::kRiscv64_ra,  .name = "ra",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_ra,  .bits = 64, .dwarf_id = 1, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kRiscv64_sp,  .name = "sp",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_sp,  .bits = 64, .dwarf_id = 2, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kRiscv64_gp,  .name = "gp",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_gp,  .bits = 64, .dwarf_id = 3, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kRiscv64_tp,  .name = "tp",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_tp,  .bits = 64, .dwarf_id = 4, .format = RegisterFormat::kWordAddress},
    {.id = RegisterID::kRiscv64_t0,  .name = "t0",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t0,  .bits = 64, .dwarf_id = 5},
    {.id = RegisterID::kRiscv64_t1,  .name = "t1",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t1,  .bits = 64, .dwarf_id = 6},
    {.id = RegisterID::kRiscv64_t2,  .name = "t2",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t2,  .bits = 64, .dwarf_id = 7},
    {.id = RegisterID::kRiscv64_s0,  .name = "s0",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s0,  .bits = 64, .dwarf_id = 8},
    {.id = RegisterID::kRiscv64_s1,  .name = "s1",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s1,  .bits = 64, .dwarf_id = 9},
    {.id = RegisterID::kRiscv64_a0,  .name = "a0",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a0,  .bits = 64, .dwarf_id = 10},
    {.id = RegisterID::kRiscv64_a1,  .name = "a1",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a1,  .bits = 64, .dwarf_id = 11},
    {.id = RegisterID::kRiscv64_a2,  .name = "a2",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a2,  .bits = 64, .dwarf_id = 12},
    {.id = RegisterID::kRiscv64_a3,  .name = "a3",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a3,  .bits = 64, .dwarf_id = 13},
    {.id = RegisterID::kRiscv64_a4,  .name = "a4",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a4,  .bits = 64, .dwarf_id = 14},
    {.id = RegisterID::kRiscv64_a5,  .name = "a5",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a5,  .bits = 64, .dwarf_id = 15},
    {.id = RegisterID::kRiscv64_a6,  .name = "a6",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a6,  .bits = 64, .dwarf_id = 16},
    {.id = RegisterID::kRiscv64_a7,  .name = "a7",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a7,  .bits = 64, .dwarf_id = 17},
    {.id = RegisterID::kRiscv64_s2,  .name = "s2",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s2,  .bits = 64, .dwarf_id = 18},
    {.id = RegisterID::kRiscv64_s3,  .name = "s3",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s3,  .bits = 64, .dwarf_id = 19},
    {.id = RegisterID::kRiscv64_s4,  .name = "s4",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s4,  .bits = 64, .dwarf_id = 20},
    {.id = RegisterID::kRiscv64_s5,  .name = "s5",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s5,  .bits = 64, .dwarf_id = 21},
    {.id = RegisterID::kRiscv64_s6,  .name = "s6",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s6,  .bits = 64, .dwarf_id = 22},
    {.id = RegisterID::kRiscv64_s7,  .name = "s7",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s7,  .bits = 64, .dwarf_id = 23},
    {.id = RegisterID::kRiscv64_s8,  .name = "s8",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s8,  .bits = 64, .dwarf_id = 24},
    {.id = RegisterID::kRiscv64_s9,  .name = "s9",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s9,  .bits = 64, .dwarf_id = 25},
    {.id = RegisterID::kRiscv64_s10, .name = "s10", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s10, .bits = 64, .dwarf_id = 26},
    {.id = RegisterID::kRiscv64_s11, .name = "s11", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s11, .bits = 64, .dwarf_id = 27},
    {.id = RegisterID::kRiscv64_t3,  .name = "t3",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t3,  .bits = 64, .dwarf_id = 28},
    {.id = RegisterID::kRiscv64_t4,  .name = "t4",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t4,  .bits = 64, .dwarf_id = 29},
    {.id = RegisterID::kRiscv64_t5,  .name = "t5",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t5,  .bits = 64, .dwarf_id = 30},
    {.id = RegisterID::kRiscv64_t6,  .name = "t6",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t6,  .bits = 64, .dwarf_id = 31},

    // General-purpose aliases.

    {.id = RegisterID::kRiscv64_x0,  .name = "x0",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_zero, .bits = 64},
    {.id = RegisterID::kRiscv64_x1,  .name = "x1",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_ra,  .bits = 64},
    {.id = RegisterID::kRiscv64_x2,  .name = "x2",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_sp,  .bits = 64},
    {.id = RegisterID::kRiscv64_x3,  .name = "x3",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_gp,  .bits = 64},
    {.id = RegisterID::kRiscv64_x4,  .name = "x4",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_tp,  .bits = 64},
    {.id = RegisterID::kRiscv64_x5,  .name = "x5",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t0,  .bits = 64},
    {.id = RegisterID::kRiscv64_x6,  .name = "x6",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t1,  .bits = 64},
    {.id = RegisterID::kRiscv64_x7,  .name = "x7",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t2,  .bits = 64},
    {.id = RegisterID::kRiscv64_x8,  .name = "x8",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s0,  .bits = 64},
    {.id = RegisterID::kRiscv64_x9,  .name = "x9",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s1,  .bits = 64},
    {.id = RegisterID::kRiscv64_x10, .name = "x10", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a0,  .bits = 64},
    {.id = RegisterID::kRiscv64_x11, .name = "x11", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a1,  .bits = 64},
    {.id = RegisterID::kRiscv64_x12, .name = "x12", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a2,  .bits = 64},
    {.id = RegisterID::kRiscv64_x13, .name = "x13", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a3,  .bits = 64},
    {.id = RegisterID::kRiscv64_x14, .name = "x14", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a4,  .bits = 64},
    {.id = RegisterID::kRiscv64_x15, .name = "x15", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a5,  .bits = 64},
    {.id = RegisterID::kRiscv64_x16, .name = "x16", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a6,  .bits = 64},
    {.id = RegisterID::kRiscv64_x17, .name = "x17", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_a7,  .bits = 64},
    {.id = RegisterID::kRiscv64_x18, .name = "x18", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s2,  .bits = 64},
    {.id = RegisterID::kRiscv64_x19, .name = "x19", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s3,  .bits = 64},
    {.id = RegisterID::kRiscv64_x20, .name = "x20", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s4,  .bits = 64},
    {.id = RegisterID::kRiscv64_x21, .name = "x21", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s5,  .bits = 64},
    {.id = RegisterID::kRiscv64_x22, .name = "x22", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s6,  .bits = 64},
    {.id = RegisterID::kRiscv64_x23, .name = "x23", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s7,  .bits = 64},
    {.id = RegisterID::kRiscv64_x24, .name = "x24", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s8,  .bits = 64},
    {.id = RegisterID::kRiscv64_x25, .name = "x25", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s9,  .bits = 64},
    {.id = RegisterID::kRiscv64_x26, .name = "x26", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s10, .bits = 64},
    {.id = RegisterID::kRiscv64_x27, .name = "x27", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_s11, .bits = 64},
    {.id = RegisterID::kRiscv64_x28, .name = "x28", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t3,  .bits = 64},
    {.id = RegisterID::kRiscv64_x29, .name = "x29", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t4,  .bits = 64},
    {.id = RegisterID::kRiscv64_x30, .name = "x30", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t5,  .bits = 64},
    {.id = RegisterID::kRiscv64_x31, .name = "x31", .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_t6,  .bits = 64},

    // PC
    {.id = RegisterID::kRiscv64_pc,  .name = "pc",  .arch = Arch::kRiscv64, .canonical_id = RegisterID::kRiscv64_pc,  .bits = 64, .dwarf_id = 64, .format = RegisterFormat::kWordAddress},
};

// clang-format on

// DWARF NOTES
//
// References
//
//   X64: https://refspecs.linuxbase.org/elf/x86_64-abi-0.99.pdf Page 57
//
//   ARM:
//   https://github.com/ARM-software/abi-aa/blob/main/aadwarf64/aadwarf64.rst#41dwarf-register-names
//
// On segment registers, we don't define any accessors for the cs, ds, es, and ss segment registers
// which must all be 0 on x64. We don't define anything for fs or gs either, these are magic
// selectors into an internal table and aren't generally useful. When user-code uses fs-relative
// addressing, this is implicitly using the fs selector to look up into a table to get "fs.base"
// which is what people actually care about. The same goes for the gs register.
//
// On x64, we use 16 (return address) to represent rip, which matches the unwinder's behavior.
//
// We don't have definitions yet of the following x86 DWARF registers:
//
//   62 -> %ts (Task Register)
//   63 -> %ldtr
//   118-125 -> %k0–%k7 (Vector Mask Registers 0–7)
//   126-129 -> %bnd0–%bnd3 (Bound Registers 0–3)
//
// Nor the following ARM DWARF registers:
//
//   33 -> ELR_mode
//   46 -> VG 64-bit SVE Vector granule pseudo register
//   47 -> FFR VG´8-bit SVE first fault register
//   48-63 -> P0-P15 VG´8-bit SVE predicate registers
//   96-127 -> Z0-Z31 VG´64-bit SVE vector registers
//
// On RISC-V, we use 64 (Alternate Frame Return Column) to represent PC so that it's consistent with
// the unwinder.

}  // namespace

const RegisterInfo* InfoForRegister(RegisterID id) {
  static std::map<RegisterID, const RegisterInfo*> info_map;

  if (info_map.empty()) {
    for (const auto& info : kRegisterInfo) {
      FX_DCHECK(info_map.find(info.id) == info_map.end());
      info_map[info.id] = &info;
    }
  }

  auto iter = info_map.find(id);
  if (iter != info_map.end())
    return iter->second;

  return nullptr;
}

const RegisterInfo* InfoForRegister(Arch arch, const std::string& name) {
  static std::map<std::pair<Arch, std::string>, const RegisterInfo*> info_map;

  if (info_map.empty()) {
    for (const auto& info : kRegisterInfo) {
      FX_DCHECK(info_map.find(std::make_pair(info.arch, info.name)) == info_map.end());
      info_map[std::make_pair(info.arch, std::string(info.name))] = &info;
    }
  }

  auto iter = info_map.find(std::make_pair(arch, name));
  if (iter != info_map.end())
    return iter->second;

  return nullptr;
}

RegisterID GetSpecialRegisterID(Arch arch, SpecialRegisterType type) {
  switch (arch) {
    case Arch::kX64:
      switch (type) {
        case SpecialRegisterType::kNone:
          break;
        case SpecialRegisterType::kIP:
          return RegisterID::kX64_rip;
        case SpecialRegisterType::kSP:
          return RegisterID::kX64_rsp;
        case SpecialRegisterType::kTP:
          return RegisterID::kX64_fsbase;
      }
      break;

    case Arch::kArm64:
      switch (type) {
        case SpecialRegisterType::kNone:
          break;
        case SpecialRegisterType::kIP:
          return RegisterID::kARMv8_pc;
        case SpecialRegisterType::kSP:
          return RegisterID::kARMv8_sp;
        case SpecialRegisterType::kTP:
          return RegisterID::kARMv8_tpidr;
      }
      break;

    case Arch::kRiscv64:
      switch (type) {
        case SpecialRegisterType::kNone:
          break;
        case SpecialRegisterType::kIP:
          return RegisterID::kRiscv64_pc;
        case SpecialRegisterType::kSP:
          return RegisterID::kRiscv64_sp;
        case SpecialRegisterType::kTP:
          return RegisterID::kRiscv64_tp;
      }
      break;

    case Arch::kUnknown:
      break;
  }

  FX_NOTREACHED();
  return RegisterID::kUnknown;
}

const char* RegisterIDToString(RegisterID id) {
  auto info = InfoForRegister(id);

  if (!info) {
    FX_NOTREACHED() << "Unknown register requested: " << static_cast<uint32_t>(id);
    return "";
  }

  return info->name.c_str();
}

RegisterID StringToRegisterID(Arch arch, const std::string& name) {
  if (auto info = InfoForRegister(arch, name)) {
    return info->id;
  }
  return RegisterID::kUnknown;
}

Arch GetArchForRegisterID(RegisterID id) {
  auto info = InfoForRegister(id);

  if (!info) {
    FX_NOTREACHED() << "Arch for unknown register requested: " << static_cast<uint32_t>(id);
    return Arch::kUnknown;
  }

  return info->arch;
}

SpecialRegisterType GetSpecialRegisterType(RegisterID id) {
  switch (id) {
    case RegisterID::kX64_rip:
    case RegisterID::kARMv8_pc:
    case RegisterID::kRiscv64_pc:
      return SpecialRegisterType::kIP;
    case RegisterID::kX64_rsp:
    case RegisterID::kARMv8_sp:
    case RegisterID::kRiscv64_sp:
      return SpecialRegisterType::kSP;
    case RegisterID::kX64_fsbase:
    case RegisterID::kARMv8_tpidr:
    case RegisterID::kRiscv64_tp:
      return SpecialRegisterType::kTP;
    default:
      return SpecialRegisterType::kNone;
  }
}

const RegisterInfo* DWARFToRegisterInfo(Arch arch, uint32_t dwarf_reg_id) {
  static std::map<std::pair<Arch, uint32_t>, const RegisterInfo*> info_map;

  if (info_map.empty()) {
    for (const auto& info : kRegisterInfo) {
      if (info.dwarf_id != RegisterInfo::kNoDwarfId) {
        FX_DCHECK(info_map.find(std::make_pair(info.arch, info.dwarf_id)) == info_map.end());
        info_map[std::make_pair(info.arch, info.dwarf_id)] = &info;
      }
    }
  }

  auto iter = info_map.find(std::make_pair(arch, dwarf_reg_id));
  if (iter != info_map.end())
    return iter->second;

  return nullptr;
}

bool IsGeneralRegister(RegisterID id) {
  return (static_cast<uint32_t>(id) >= static_cast<uint32_t>(kARMv8GeneralBegin) &&
          static_cast<uint32_t>(id) <= static_cast<uint32_t>(kARMv8GeneralEnd)) ||
         (static_cast<uint32_t>(id) >= static_cast<uint32_t>(kX64GeneralBegin) &&
          static_cast<uint32_t>(id) <= static_cast<uint32_t>(kX64GeneralEnd)) ||
         (static_cast<uint32_t>(id) >= static_cast<uint32_t>(kRiscv64GeneralBegin) &&
          static_cast<uint32_t>(id) <= static_cast<uint32_t>(kRiscv64GeneralEnd));
}

const char* RegisterCategoryToString(RegisterCategory cat) {
  switch (cat) {
    case RegisterCategory::kGeneral:
      return "General Purpose";
    case RegisterCategory::kFloatingPoint:
      return "Floating Point";
    case RegisterCategory::kVector:
      return "Vector";
    case RegisterCategory::kDebug:
      return "Debug";
    case RegisterCategory::kNone:
    case RegisterCategory::kLast:
      break;
  }
  FX_NOTREACHED();
  return nullptr;
}

RegisterCategory RegisterIDToCategory(RegisterID id) {
  uint32_t val = static_cast<uint32_t>(id);

  // ARM.
  if (val >= kARMv8GeneralBegin && val <= kARMv8GeneralEnd) {
    return RegisterCategory::kGeneral;
  } else if (val >= kARMv8VectorBegin && val <= kARMv8VectorEnd) {
    return RegisterCategory::kVector;
  } else if (val >= kARMv8DebugBegin && val <= kARMv8DebugEnd) {
    return RegisterCategory::kDebug;
  }

  // x64.
  if (val >= kX64GeneralBegin && val <= kX64GeneralEnd) {
    return RegisterCategory::kGeneral;
  } else if (val >= kX64FPBegin && val <= kX64FPEnd) {
    return RegisterCategory::kFloatingPoint;
  } else if (val >= kX64VectorBegin && val <= kX64VectorEnd) {
    return RegisterCategory::kVector;
  } else if (val >= kX64DebugBegin && val <= kX64DebugEnd) {
    return RegisterCategory::kDebug;
  }

  // RISC-V 64.
  if (val >= kRiscv64GeneralBegin && val <= kRiscv64GeneralEnd) {
    return RegisterCategory::kGeneral;
  } else if (val >= kRiscv64FPBegin && val <= kRiscv64FPEnd) {
    return RegisterCategory::kFloatingPoint;
  } else if (val >= kRiscv64VectorBegin && val <= kRiscv64VectorEnd) {
    return RegisterCategory::kVector;
  } else if (val >= kRiscv64DebugBegin && val <= kRiscv64DebugEnd) {
    return RegisterCategory::kDebug;
  }

  return RegisterCategory::kNone;
}

cpp20::span<const uint8_t> GetRegisterData(const std::vector<RegisterValue>& regs, RegisterID id) {
  const RegisterInfo* info = InfoForRegister(id);
  if (!info)
    return cpp20::span<uint8_t>();

  const RegisterValue* found_canonical = nullptr;
  for (const auto& reg : regs) {
    if (reg.id == id)
      return reg.data;  // Prefer an exact match.
    if (reg.id == info->canonical_id) {
      found_canonical = &reg;
      break;
    }
  }

  if (!found_canonical)
    return cpp20::span<uint8_t>();

  // Here we found a canonical register match that's not the exact register being requested. Extract
  // the correct number of bits.

  // Expect everything to be a multiple of 8. Currently all of our processors' pseudoregisters have
  // this property.
  FX_DCHECK(info->bits > 0);
  FX_DCHECK(info->bits % 8 == 0);
  FX_DCHECK(info->shift % 8 == 0);

  cpp20::span<const uint8_t> result = found_canonical->data;

  // The shift is a trim from the left because we assume little-endian.
  return result.subspan(info->shift / 8, info->bits / 8);
}

}  // namespace debug
