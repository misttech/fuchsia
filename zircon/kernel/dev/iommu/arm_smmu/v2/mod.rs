// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 register blocks, stream matching, context banks, SMMU controller, BTI, and PMT.
//!
//! SMMUv2-specific hardware management lives in `v2/`, while hardware-independent page-table and
//! address-space infrastructure lives in `common/` for future reuse by SMMUv3. Submodule items are
//! re-exported here to preserve a flat namespace surface within `v2`.

pub mod context_bank;
pub mod registers;
pub mod smmu;
pub mod smmu_bti;
pub mod smmu_pmt;
pub mod stream_match_reg;
pub mod utils;

pub use context_bank::ContextBank;
pub use registers::*;
pub use smmu::{
    ConsoleArg, IrqDef, Smmu, ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH,
    ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW, ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE,
    ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE, ZBI_ARM_SMMU_MAX_HANDOFF_SMRS, ZBI_ARM_SMMU_MAX_IRQS,
    ZbiDcfgArmSmmuDriver, ZbiDcfgArmSmmuIrq, ZxIommuDescArmSmmu,
};
pub use smmu_bti::SmmuBti;
pub use smmu_pmt::{SmmuPmt, SmmuPmtState};
pub use stream_match_reg::StreamMatchRegGroup;
pub use utils::{
    AddrMode, Bitmask128, BtiMode, SmmuController, SmmuParams, SmrValue, SmrValueIter,
    addr_mode_to_string, arm_cbar_type_to_string, arm_s2cr_type_to_string, arm_smmu_mode_to_string,
    bti_mode_to_string, smr_intersects,
};
