// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! SMMUv2 utility types and helpers: [`Bitmask128`], [`SmrValue`] stream-matching mask/ID value
//! and iterator, string formatting helpers for SMMUv2 modes and register types, and the
//! [`SmmuController`] interface shared across [`super::stream_match_reg::StreamMatchRegGroup`] and
//! [`super::context_bank::ContextBank`].

use zx_status::Status;

use crate::arm_smmu::common::ArmSmmuMode;
use crate::arm_smmu::v2::registers::SmmuMmio;
use crate::arm_smmu::v2::registers::gr0::{S2crType, Smr};
use crate::arm_smmu::v2::registers::gr1::CbarType;

/// Fixed-capacity 128-bit allocation and tracking bitmask for SMMUv2 Stream Match Register Groups
/// (up to 128 SMRGs) and Context Banks (up to 128 CBs).
///
/// Uses native `u128` bit intrinsics (`count_ones`, `trailing_zeros`, `trailing_ones`) in a single
/// word because the SMMUv2 architectural maximum for both SMRGs and Context Banks is 128.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bitmask128(u128);

impl Bitmask128 {
    /// Total number of bits managed by [`Bitmask128`].
    pub const NUM_BITS: u32 = 128;

    /// Constructs a new [`Bitmask128`] with all bits cleared (`0`).
    #[inline]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Constructs a [`Bitmask128`] from a raw `u128` bit pattern.
    #[inline]
    pub const fn from_raw(raw: u128) -> Self {
        Self(raw)
    }

    /// Returns the raw `u128` bit pattern.
    #[inline]
    pub const fn raw(&self) -> u128 {
        self.0
    }

    /// Sets bit `bit` (`0 <= bit < 128`) to `1`.
    #[inline]
    pub fn set_bit(&mut self, bit: u32) {
        debug_assert!(bit < Self::NUM_BITS);
        self.0 |= 1u128 << bit;
    }

    /// Clears bit `bit` (`0 <= bit < 128`) to `0`.
    #[inline]
    pub fn clear_bit(&mut self, bit: u32) {
        debug_assert!(bit < Self::NUM_BITS);
        self.0 &= !(1u128 << bit);
    }

    /// Assigns bit `bit` (`0 <= bit < 128`) to `val`.
    #[inline]
    pub fn assign_bit(&mut self, bit: u32, val: bool) {
        if val {
            self.set_bit(bit);
        } else {
            self.clear_bit(bit);
        }
    }

    /// Returns `true` if bit `bit` (`0 <= bit < 128`) is set.
    #[inline]
    pub const fn test_bit(&self, bit: u32) -> bool {
        debug_assert!(bit < Self::NUM_BITS);
        (self.0 & (1u128 << bit)) != 0
    }

    /// Sets the lowest `n` bits (`0 <= n <= 128`) to `1` (ORing them into the existing mask).
    #[inline]
    pub fn set_lowest_n_bits(&mut self, n: u32) {
        debug_assert!(n <= Self::NUM_BITS);
        // Temporary porting note: the C++ `Bitmask<128>::SetLowestNBits(128)` indexed one element
        // past the end of its storage array (harmlessly ORing in zero). `n == 128`, which is what
        // an SMMU with the architectural maximum of 128 SMRGs or context banks produces, is
        // handled explicitly here.
        if n == Self::NUM_BITS {
            self.0 = u128::MAX;
        } else if n > 0 {
            self.0 |= (1u128 << n) - 1;
        }
    }

    /// Sets all 128 bits to `1`.
    #[inline]
    pub fn set_all(&mut self) {
        self.0 = u128::MAX;
    }

    /// Clears all 128 bits to `0`.
    #[inline]
    pub fn clear_all(&mut self) {
        self.0 = 0;
    }

    /// Returns the number of bits currently set to `1`.
    #[inline]
    pub const fn num_set_bits(&self) -> u32 {
        self.0.count_ones()
    }

    /// Returns the index (`0..128`) of the least-significant set bit, or `None` if all bits are
    /// zero.
    #[inline]
    pub const fn find_first_set_bit(&self) -> Option<u32> {
        if self.0 == 0 { None } else { Some(self.0.trailing_zeros()) }
    }

    /// Returns the index (`0..128`) of the least-significant clear bit, or `None` if all bits are
    /// one.
    #[inline]
    pub const fn find_first_clr_bit(&self) -> Option<u32> {
        if self.0 == u128::MAX { None } else { Some(self.0.trailing_ones()) }
    }

    /// Returns an iterator yielding the bit indices (`0..128`) of all set bits in ascending order.
    #[inline]
    pub const fn iter_set_bits(&self) -> Bitmask128Iter {
        Bitmask128Iter { remaining: self.0 }
    }
}

/// Iterator over the set bit indices (`0..128`) of a [`Bitmask128`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bitmask128Iter {
    remaining: u128,
}

impl Iterator for Bitmask128Iter {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let bit = self.remaining.trailing_zeros();
        self.remaining &= self.remaining - 1;
        Some(bit)
    }
}

impl IntoIterator for Bitmask128 {
    type Item = u32;
    type IntoIter = Bitmask128Iter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_set_bits()
    }
}

impl IntoIterator for &Bitmask128 {
    type Item = u32;
    type IntoIter = Bitmask128Iter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter_set_bits()
    }
}

/// The addressing mode for a context bank determined using values in CBA2R and TCR.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrMode {
    /// AArch32 Short-descriptor translation table format.
    Mode32Bit = 0,
    /// AArch32 Long-descriptor translation table format.
    Ext32Bit = 1,
    /// AArch64 VMSAv8-64 translation table format.
    Mode64Bit = 2,
    /// Unrecognized or invalid addressing mode configuration.
    Invalid = 3,
}

/// Operational mode for an `SmmuBti` or `ContextBank`.
///
/// This enum is used in two places, at the BTI level, and at the context bank level for the
/// (single) context bank owned by a BTI. Note that when operating in fully enforced mode, the BTI
/// may be in the Fault state (preventing new pin operations) while its ContextBank remains in
/// Translation mode in order to continue to allow access to actively pinned PMTs, while
/// specifically denying access to leaked PMT regions.
///
/// In the context of a Context bank, the values used in this enum are not specific to any one
/// register value. Instead, they reflect the way that we choose to model the three modes of
/// operation using the CBAR (gr1), SCTLR (cb(N)), and TCR (cb(N)) registers.
///
/// Their definitions for each mode are as follows:
///
/// ```text
/// Mode        | CBAR.TYPE   | SCTLR.M | TCR.EPD0 | TCR.EPD1 | Notes
/// ------------+-------------+---------+----------+----------+---------------------------------
/// Translation | S1TS2Bypass |    1    |    0     |    1     | MMU enabled, TTBR0 enabled
/// Bypass      | S1TS2Bypass |    0    |    1     |    1     | MMU + TTBRs Disabled
/// Fault       | S1TS2Bypass |    1    |    0     |    0     | MMU enabled, TTBRs disabled
/// Adopted     |    ???      |   ???   |   ???    |   ???    | Register config at adoption.
/// Shutdown    | S1TS2Bypass |    1    |    0     |    0     | Same as fault, deny all access.
/// ```
///
/// Note that all of the modes (except adopted) use Stage 1 Translate, Stage 2 Bypass as their
/// CBAR type. Stage 2's configuration is frequently under the control of either the hypervisor or
/// the secure monitor, which may deny any attempt to put the system into a S1TS2Fault mode in
/// order to force faulting. Additionally, while we might specify either S2Bypass or S2Translate,
/// we typically have no control over Stage 2 behavior from EL1. We configure for S2Bypass,
/// assuming that there is no S2 translation going on, however it may be the case that the system
/// chooses to either change our written value to S1TS2Translate if it wants to perform
/// translation, or to lie to us by reporting that our CBAR.TYPE is S1TS2Bypass even though (under
/// the hood) it has actually configured for Stage 2 Translation.
///
/// Either way, we are still able to represent all three of the primary modes with S1TS2Bypass. For
/// full translation, we enable the MMU, and TTBR0 which is configured to point to our page tables.
/// TTBR1 is not currently used in any mode. For bypass mode, we disable the MMU, which means that
/// all transactions which reach this context bank are simply accepted and passed through as is
/// with no translation. Finally, when configured for Fault/Shutdown, the MMU is enabled, but both
/// TTBRs are disabled, meaning there are no valid/active translation table entries, ensuring that
/// "translation" in Fault mode always fails.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtiMode {
    /// Fault mode: A fault has occurred. Either a PMT has been leaked, or the HW attempted to
    /// access something it didn't have access to and we noticed. Access is restricted until
    /// user-mode takes control of their HW and signals that it has by calling
    /// `zx_bti_release_quarantine`.
    Fault = 0,
    /// Passthru mode: No translation is performed, all accesses are allowed.
    Bypass = 1,
    /// Enforced mode: Translation is performed, only pinned memory can be accessed.
    Translation = 2,
    /// Adopted mode: The configuration is whatever was passed to us by our bootloader.
    Adopted = 3,
    /// Shutdown: The BTI has been shutdown. HW should be in Fault mode, but cannot ever return to
    /// an operational mode. The BTI is about to be destroyed.
    Shutdown = 4,
    /// Uninitialized or invalid mode.
    Invalid = 5,
}

/// Returns the human-readable string representation of an [`ArmSmmuMode`].
#[inline]
pub const fn arm_smmu_mode_to_string(mode: ArmSmmuMode) -> &'static str {
    match mode {
        ArmSmmuMode::Disabled => "Disabled",
        ArmSmmuMode::Passthru => "Passthru",
        ArmSmmuMode::Enforced => "Enforced",
    }
}

/// Returns the human-readable string representation of a [`CbarType`].
#[inline]
pub const fn arm_cbar_type_to_string(cbar_type: CbarType) -> &'static str {
    cbar_type.as_str()
}

/// Returns the human-readable string representation of an [`S2crType`].
#[inline]
pub const fn arm_s2cr_type_to_string(s2cr_type: S2crType) -> &'static str {
    s2cr_type.as_str()
}

/// Returns the human-readable string representation of an [`AddrMode`].
#[inline]
pub const fn addr_mode_to_string(mode: AddrMode) -> &'static str {
    match mode {
        AddrMode::Mode32Bit => "32Bit",
        AddrMode::Ext32Bit => "Ext32Bit",
        AddrMode::Mode64Bit => "64Bit",
        AddrMode::Invalid => "Invalid",
    }
}

/// Returns the human-readable string representation of a [`BtiMode`].
#[inline]
pub const fn bti_mode_to_string(mode: BtiMode) -> &'static str {
    match mode {
        BtiMode::Fault => "Fault",
        BtiMode::Bypass => "Bypass",
        BtiMode::Translation => "Translation",
        BtiMode::Adopted => "Adopted",
        BtiMode::Shutdown => "Shutdown",
        BtiMode::Invalid => "Invalid",
    }
}

/// Encoded Stream Match Register (`SMRn`) `(MASK << 16) | ID` value.
///
/// [`SmrValue`] is a helper struct which manages the value contained in a Stream Match Register,
/// and provides useful manipulation and query operations on the value. Notably, SMRs contain both
/// a 15-bit ID field in bits `[14:0]`, as well as a 15-bit MASK field in bits `[30:16]`. This sets
/// up a ternary match pattern where each bit in the mask/value can effectively have one of 3
/// values (`0`, `1`, or `X`, where `X` is a "don't care" wildcard).
///
/// Operations provided by [`SmrValue`] include:
/// + Constructing a masked [`SmrValue`] from a raw `u32` (coming either from a register read, or
///   an encoded `bti_id` requested by a user), clearing any bits outside of the valid mask
///   determined by the hardware configuration.
/// + Extracting the `ID` or `MASK` fields individually.
/// + Testing whether two [`SmrValue`] instances intersect (match at least one common Stream ID).
/// + Iterating over the set of all discrete 15-bit Stream IDs matched by the [`SmrValue`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmrValue {
    value: u32,
}

impl Default for SmrValue {
    #[inline]
    fn default() -> Self {
        Self::default_invalid()
    }
}

impl SmrValue {
    /// Maximum 15-bit value for either the `ID` or `MASK` field (`0x7fff`).
    pub const MAX_VALUE: u16 = 0x7fff;

    /// Canonical valid mask covering both 15-bit `MASK` (`[30:16]`) and `ID` (`[14:0]`) fields.
    pub const FULL_VALID_MASK: u32 = ((Self::MAX_VALUE as u32) << 16) | (Self::MAX_VALUE as u32);

    /// Returns the default uninitialized [`SmrValue`] (`0x7fff_ffff`).
    #[inline]
    pub const fn default_invalid() -> Self {
        Self { value: 0x7fff_ffff }
    }

    /// Constructs an [`SmrValue`] from a raw `(MASK << 16) | ID` register value and `valid_mask`.
    ///
    /// Only bits outside of `valid_mask` are cleared. In particular, as in the C++ `SmrValue`, any
    /// bits of `ID` which are also set in `MASK` are preserved exactly as given, so [`Self::value`]
    /// round-trips the `bti_id` passed by a user or the contents of an adopted `SMR` register.
    /// Such bits are ignored by the hardware when matching (and by [`Self::intersects`] and
    /// [`Self::contains`]); the only place they need to be cleared is when enumerating the
    /// matched Stream IDs, which [`SmrValueIter`] takes care of.
    #[inline]
    pub const fn from_raw(reg_val: u32, valid_mask: u32) -> Self {
        // Note, the only place we would ever expect to see a `reg_val` with bits set outside of
        // the valid mask would be when using the kernel console to execute a `show` command with a
        // target specified by SID in order to create an `SmrValue` instance to use when searching
        // for a BTI target. Aside from that, the `reg_val` given here should always exist within
        // the valid mask as determined by HW configuration.
        Self { value: reg_val & valid_mask }
    }

    /// Constructs an [`SmrValue`] from explicit 15-bit `id` and `mask` components.
    #[inline]
    pub const fn new(id: u16, mask: u16) -> Self {
        let raw = ((mask as u32) << 16) | (id as u32);
        Self::from_raw(raw, Self::FULL_VALID_MASK)
    }

    /// Validates and constructs an [`SmrValue`] from a 64-bit `bus_txn_id` (`bti_id`) and
    /// hardware `valid_stream_id_mask`.
    ///
    /// Returns `Err(Status::INVALID_ARGS)` if `bti_id` sets any bit outside `valid_mask`.
    #[inline]
    pub const fn from_bti_id(bti_id: u64, valid_mask: u32) -> Result<Self, Status> {
        if (bti_id & !(valid_mask as u64)) != 0 {
            return Err(Status::INVALID_ARGS);
        }
        Ok(Self::from_raw(bti_id as u32, valid_mask))
    }

    /// Constructs an [`SmrValue`] from an [`Smr`] register snapshot and `valid_mask`.
    #[inline]
    pub const fn from_smr_reg(smr: Smr, valid_mask: u32) -> Self {
        Self::from_raw(smr.reg_value(), valid_mask)
    }

    /// Returns the raw `(MASK << 16) | ID` value.
    #[inline]
    pub const fn value(&self) -> u32 {
        self.value
    }

    /// Extracts the 15-bit `ID` field (`bits [14:0]`), exactly as given at construction (bits
    /// which are also set in `MASK` are not cleared).
    #[inline]
    pub const fn id(&self) -> u16 {
        ((self.value & 0xffff) as u16) & Self::MAX_VALUE
    }

    /// Returns the lowest Stream ID matched by this value: the `ID` field with every bit that is
    /// wildcarded by `MASK` cleared.
    #[inline]
    pub const fn first_stream_id(&self) -> u16 {
        self.id() & !self.mask()
    }

    /// Extracts the 15-bit `MASK` field (`bits [30:16]`).
    #[inline]
    pub const fn mask(&self) -> u16 {
        (((self.value >> 16) & 0xffff) as u16) & Self::MAX_VALUE
    }

    /// Returns `true` if `self` and `other` match at least one common Stream ID.
    ///
    /// Checks whether the unmasked bits in both [`SmrValue`]s are identical. A bit position only
    /// needs to match when it is unmasked (`0` in `MASK`) in *both* `self` and `other`.
    #[inline]
    pub const fn intersects(&self, other: Self) -> bool {
        let combined_mask = self.mask() | other.mask();
        ((self.id() ^ other.id()) & !combined_mask) == 0
    }

    /// Returns `true` if `self` matches the discrete 15-bit `stream_id`.
    #[inline]
    pub const fn contains(&self, stream_id: u16) -> bool {
        ((self.id() ^ (stream_id & Self::MAX_VALUE)) & !self.mask()) == 0
    }

    /// Returns the total number of discrete Stream IDs matched by `self`
    /// (`1 << mask.count_ones()`).
    #[inline]
    pub const fn num_streams(&self) -> u32 {
        1u32 << self.mask().count_ones()
    }

    /// Returns an iterator yielding every discrete 15-bit Stream ID matched by `self` in ascending
    /// order.
    #[inline]
    pub const fn iter(&self) -> SmrValueIter {
        SmrValueIter::new(*self)
    }
}

/// Tests to see if two SMRs have any potential to match the same specific stream ID based on the
/// encoded id/mask pair.
#[inline]
pub const fn smr_intersects(smr1: u32, smr2: u32) -> bool {
    // Compute `fixed_bits`, which is the set of bits that both SMRs care about (as opposed to the
    // "don't-care" bits defined by the SMR mask). All of the bits set to 1 in `fixed_bits` should
    // be the bits which are zero in *both* of the SMRs' "mask" fields.
    //
    // As in C++, this works on the full 16-bit halves of the raw values, without first clamping
    // them to the 15-bit `ID` and `MASK` fields the way `SmrValue` does.
    let fixed_bits = (!smr1 >> 16) & (!smr2 >> 16);

    // If the "value" fields of each SMR masked by the `fixed_bits` we just computed match, then
    // there must exist at least one Stream ID which is considered to be valid by both of the
    // SMRs, and the two SMRs are said to "intersect".
    (smr1 & fixed_bits) == (smr2 & fixed_bits)
}

/// Iterator over all discrete 15-bit Stream IDs matched by an [`SmrValue`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmrValueIter {
    owner: SmrValue,
    current: u16,
}

impl SmrValueIter {
    /// Sentinel value indicating that the iterator has reached the end of the sequence.
    const END_SENTINEL: u16 = 0xffff;

    /// Constructs a new iterator starting at the lowest Stream ID (`owner.first_stream_id()`).
    #[inline]
    pub const fn new(owner: SmrValue) -> Self {
        Self { owner, current: owner.first_stream_id() }
    }
}

impl Iterator for SmrValueIter {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.current == Self::END_SENTINEL {
            return None;
        }
        let ret = self.current;

        // Advance `current` to the next Stream ID matching `owner` in O(1) using masked
        // ripple-carry addition (rather than scattering index bits across set bits of `mask` in an
        // O(popcount) loop on each step), yielding the exact same sequence in ascending order:
        // 1. Set all fixed (unmasked) bit positions to 1 via `current | !m`.
        // 2. Add 1 so carries ripple across fixed bit positions directly into the next higher
        //    masked bit position.
        // 3. Mask with `m` to keep only the incremented wildcard bits and OR back the fixed bits.
        //
        // The fixed bits come from `owner.first_stream_id()` rather than `owner.id()`: an
        // `SmrValue` keeps its `ID` field exactly as given, which may have bits set that `MASK`
        // wildcards. Those must be cleared here, both so that every wildcard combination is
        // produced and so that the wrap-around test below terminates the iteration. (The C++
        // iterator did the same with `value_ & ~mask_`.)
        let m = self.owner.mask() as u32;
        let first = self.owner.first_stream_id();
        let next_val = ((((self.current as u32) | !m).wrapping_add(1)) & m) | (first as u32);
        let next_u16 = next_val as u16;

        // If `next_u16` wrapped back around to `first`, we have visited all `2^popcount(m)`
        // matching Stream IDs.
        if next_u16 == first {
            self.current = Self::END_SENTINEL;
        } else {
            self.current = next_u16;
        }
        Some(ret)
    }
}

impl IntoIterator for SmrValue {
    type Item = u16;
    type IntoIter = SmrValueIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for &SmrValue {
    type Item = u16;
    type IntoIter = SmrValueIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Interface providing read-only access to SMMUv2 controller configuration, MMIO banks, and
/// resource availability bitmasks required by [`super::stream_match_reg::StreamMatchRegGroup`] and
/// [`super::context_bank::ContextBank`].
///
/// Methods taking `&impl SmmuController` correspond to operations that require the controller lock
/// (`TA_REQ(smmu.get_lock())`). Production callers construct [`SmmuParams`] via
/// `Smmu::controller_params(&guard)`, which statically requires holding `SmmuLockGuard`, while
/// standalone unit tests can construct [`SmmuParams`] directly over a fake MMIO buffer without
/// instantiating a full `Smmu`.
pub trait SmmuController {
    /// Returns the diagnostic name of the SMMU instance (e.g. `"SMMU@0x04e54000"`).
    fn name(&self) -> &str;

    /// Returns the operating mode of the SMMU instance.
    fn op_mode(&self) -> ArmSmmuMode;

    /// Returns the total number of Stream Match Register Groups managed by this SMMU.
    fn num_smrgs(&self) -> u32;

    /// Returns the total number of Context Banks managed by this SMMU.
    fn num_cbs(&self) -> u32;

    /// Returns the bitmask of currently available (unallocated) Stream Match Register Groups.
    fn available_smrgs(&self) -> &Bitmask128;

    /// Returns the bitmask of currently available (unallocated) Context Banks.
    fn available_cbs(&self) -> &Bitmask128;

    /// Returns the MMIO handle for Global Register Space 0 (`SMMU_GR0_BASE`).
    fn gr0_base(&self) -> SmmuMmio;

    /// Returns the MMIO handle for Global Register Space 1 (`SMMU_GR1_BASE`).
    fn gr1_base(&self) -> SmmuMmio;

    /// Returns the MMIO handle for Context Bank `cb_ndx` (`SMMU_CBn_BASE`).
    fn get_cb_base(&self, cb_ndx: u32) -> SmmuMmio;
}

/// Lightweight value bundle implementing [`SmmuController`] for both locked production `Smmu`
/// calls (via `Smmu::controller_params(&guard)`) and standalone host/kernel unit tests.
#[derive(Clone, Copy, Debug)]
pub struct SmmuParams<'a> {
    /// Diagnostic name of the SMMU instance.
    pub name: &'a str,
    /// Operating mode of the SMMU instance.
    pub op_mode: ArmSmmuMode,
    /// Total number of Stream Match Register Groups.
    pub num_smrgs: u32,
    /// Total number of Context Banks.
    pub num_cbs: u32,
    /// Bitmask of available Stream Match Register Groups.
    pub available_smrgs: &'a Bitmask128,
    /// Bitmask of available Context Banks.
    pub available_cbs: &'a Bitmask128,
    /// MMIO handle for Global Register Space 0 (`GR0`).
    pub gr0_base: SmmuMmio,
    /// MMIO handle for Global Register Space 1 (`GR1`).
    pub gr1_base: SmmuMmio,
    /// MMIO handle for Context Bank 0 (`CB0`).
    pub cb0_base: SmmuMmio,
    /// Size in bytes of each register page (`0x1000` or `0x10000`).
    pub reg_page_size: usize,
}

impl SmmuController for SmmuParams<'_> {
    #[inline]
    fn name(&self) -> &str {
        self.name
    }

    #[inline]
    fn op_mode(&self) -> ArmSmmuMode {
        self.op_mode
    }

    #[inline]
    fn num_smrgs(&self) -> u32 {
        self.num_smrgs
    }

    #[inline]
    fn num_cbs(&self) -> u32 {
        self.num_cbs
    }

    #[inline]
    fn available_smrgs(&self) -> &Bitmask128 {
        self.available_smrgs
    }

    #[inline]
    fn available_cbs(&self) -> &Bitmask128 {
        self.available_cbs
    }

    #[inline]
    fn gr0_base(&self) -> SmmuMmio {
        self.gr0_base
    }

    #[inline]
    fn gr1_base(&self) -> SmmuMmio {
        self.gr1_base
    }

    #[inline]
    fn get_cb_base(&self, cb_ndx: u32) -> SmmuMmio {
        debug_assert!(cb_ndx < self.num_cbs);
        self.cb0_base.sub_bank((cb_ndx as usize) * self.reg_page_size, self.reg_page_size)
    }
}
