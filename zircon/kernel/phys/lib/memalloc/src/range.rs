// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! A memory range type used in the representation of the internal memory map managed by Memalloc.
//!
//! This module introduces the [`Range`] type, which represents a contiguous chunk of physical
//! memory.  Each range has a [`Type`] associated with it, which describes what the memory is used
//! for.
//!
//! The [`NormalizedRangesIterator`] subtrait is introduced to indicate that an iterator over a
//! collection of ranges behaves nicely.

use core::cmp::Ordering;
use core::mem::offset_of;
use core::slice;

use zr::static_assert;

/// Helper macro for defining the [`Type`] enum to assume the discriminants from C++ bindgen.
macro_rules! define_type_enum {
    (
        $(#[$enum_attr:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$variant_attr:meta])*
                $variant:ident
            ),* $(,)?
        }
    ) => {
        paste::paste! {
            $(#[$enum_attr])*
            $vis enum $name {
                $(
                    $(#[$variant_attr])*
                    $variant = bindings::memalloc_Type::[<k $variant>] as u64,
                )*
            }
        }
    };
}

define_type_enum! {
    /// A description of a physical memory range.
    ///
    /// A few important sub-categories of the `Type` enum:
    ///
    ///     * Base types. These are the three types specified by the ZBI: `Type::FreeRam`,
    ///     `Type::Peripheral`, and `Type::Reserved`.
    ///
    ///     * Allocated types.  These are all of the types which are specific to Memalloc's use
    ///     case, and are not present in the ZBI spec.  For example, `Type::PoolBookkeeping`,
    ///     `Type::PhysKernel`, etc.  These types are all carved out from `Type::FreeRam`.
    ///
    ///     * Ram types.  This encompasses all of the allocated types plus `Type::FreeRam`
    ///
    /// `Type` is represented by 64 bits.
    ///
    /// The lower 2^32 values in the space are reserved for the base types; the upper values are
    /// allocated types.
    ///
    /// The C++ definition of this enum serves as the source of truth for all of the discriminant
    /// values.  We enforce this in Rust indirectly by assigning the variants of the enum to the
    /// C++ values, with bindgen.  The [`define_type_enum!`] macro makes this assignment automatic.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u64)]
    pub enum Type {
        FreeRam,
        Peripheral,
        Reserved,
        /// Reserved for internal bookkeeping.
        PoolBookkeeping,
        /// The phys ZBI kernel memory image.
        PhysKernel,
        /// A phys ELF memory image.
        PhysElf,
        /// A phys log file.
        PhysLog,
        /// The kernel memory image.
        Kernel,
        /// A (decompressed) STORAGE_KERNEL ZBI payload.
        KernelStorage,
        /// The data ZBI, as placed by the bootloader.
        DataZbi,
        /// Memory intended to remain allocated until the end of the phys hand-off phase.
        TemporaryPhysHandoff,
        /// Memory intended to remain allocated for the lifetime of the kernel.
        PermanentPhysHandoff,
        /// A vDSO memory image.
        Vdso,
        /// A userboot memory image.
        Userboot,
        /// The kernel's boot machine stack.
        BootMachineStack,
        /// The kernel's boot shadow call stack, if supported.
        BootShadowCallStack,
        /// The kernel's boot unsafe stack, if safe stack is supported.
        BootUnsafeStack,
        /// The intermediate kernel memory image used to trampoline into the same image loaded at a
        /// fixed address (i.e., as used by FixedAddressBootZbi).
        FixedAddressStagingKernel,
        /// The intermediate data ZBI used to trampoline into the same image loaded at a fixed
        /// address (i.e., as used by FixedAddressBootZbi).
        FixedAddressStagingDataZbi,
        /// Data structures related to legacy boot protocols.
        LegacyBootData,
        /// Identity-mapping page tables intended only for the lifetime of the phys program in
        /// execution.
        TemporaryIdentityPageTables,
        /// Page tables that describe mappings intended to exist into the kernel's proper lifetime.
        KernelPageTables,
        /// A debug data blob of phys origin (e.g., related to instrumentation).
        PhysDebugdata,
        /// A firmware-provided devicetree blob.
        DevicetreeBlob,
        /// General scratch space used by the phys kernel, but that which is free for the next
        /// kernel as of hand-off.
        PhysScratch,
        /// A generic allocated type for Pool tests.
        PoolTestPayload,
        /// A generic allocated type for ZBI tests.
        ZbiTestPayload,
        /// Memory carved out for the kernel.test.ram.reserve boot option.
        TestRamReserve,
        /// Memory carved out for the ZBI_TYPE_NVRAM region.
        Nvram,
        /// Low memory, in an architecture-specific context, that is deemed to be unsafe for
        /// general-purpose allocation and use (e.g., BIOS-related bits below 1MiB in the case of
        /// PCs).
        ReservedLow,
        /// RAM 'discarded' from a truncation of the physical address space when simulating booting
        /// contexts with less physical memory available.
        TruncatedRam,
    }
}

// Ensure `Type` has the same layout as the C++ version, using bindgen.
static_assert!(size_of::<Type>() == size_of::<bindings::memalloc_Type>());
static_assert!(align_of::<Type>() == align_of::<bindings::memalloc_Type>());

impl Type {
    /// Memalloc reserves the discriminant values above 2^32 to enumerate its own types.
    pub const MIN_ALLOCATED_VALUE: u64 = bindings::memalloc_kMinAllocatedTypeValue;

    /// Convert a `Type` to a string slice.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Type::FreeRam => "free RAM",
            Type::Reserved => "reserved",
            Type::Peripheral => "peripheral",
            Type::PoolBookkeeping => "bookkeeping",
            Type::PhysKernel => "phys ZBI kernel image",
            Type::PhysElf => "phys ELF image",
            Type::PhysLog => "phys log file",
            Type::Kernel => "kernel image",
            Type::KernelStorage => "decompressed kernel payload",
            Type::DataZbi => "data ZBI",
            Type::TemporaryPhysHandoff => "phys hand-off data (temporary)",
            Type::PermanentPhysHandoff => "phys hand-off data (permanent)",
            Type::Vdso => "vDSO",
            Type::Userboot => "userboot",
            Type::BootMachineStack => "boot machine stack",
            Type::BootShadowCallStack => "boot shadow call stack",
            Type::BootUnsafeStack => "boot unsafe stack",
            Type::FixedAddressStagingKernel => "trampoline staging kernel image",
            Type::FixedAddressStagingDataZbi => "trampoline staging data ZBI",
            Type::LegacyBootData => "legacy boot data",
            Type::TemporaryIdentityPageTables => "temporary identity page tables",
            Type::KernelPageTables => "kernel page tables",
            Type::PhysDebugdata => "phys debugdata",
            Type::DevicetreeBlob => "devicetree blob",
            Type::PhysScratch => "phys scratch",
            Type::PoolTestPayload => "memalloc::Pool test payload",
            Type::ZbiTestPayload => "ZBI test payload",
            Type::TestRamReserve => "kernel.test.ram.reserve",
            Type::Nvram => "ZBI_TYPE_NVRAM",
            Type::ReservedLow => "reserved low memory",
            Type::TruncatedRam => "truncated RAM",
        }
    }

    /// Determine if a `Type` is an allocated type.
    pub const fn is_allocated(&self) -> bool {
        (*self as u64) >= Self::MIN_ALLOCATED_VALUE
    }

    /// Determine if a `Type` is a ram type.
    pub const fn is_ram(&self) -> bool {
        matches!(*self, Type::FreeRam) || self.is_allocated()
    }
}

/// A physical memory range type.
///
/// `Range` specifies a half open iterval of physical memory `[addr, addr + size)` with `ty`
/// describing the usage of this memory.  It layout-compatible to `zbi_mem_range_t`, but with the
/// benefit of being able to use allocated types.
///
/// `addr + size` never overflows.
#[repr(C)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Range {
    addr: u64,
    size: u64,
    ty: Type,
}

// Ensure `Range` has the same layout as the C++ version.
static_assert!(size_of::<Range>() == size_of::<bindings::memalloc_Range>());
static_assert!(align_of::<Range>() == align_of::<bindings::memalloc_Range>());
static_assert!(offset_of!(Range, addr) == offset_of!(bindings::memalloc_Range, addr));
static_assert!(offset_of!(Range, size) == offset_of!(bindings::memalloc_Range, size));
static_assert!(offset_of!(Range, ty) == offset_of!(bindings::memalloc_Range, type_));

// Ensure `Range` has the same layout as `zbi::MemRange`.
static_assert!(size_of::<Range>() == size_of::<zbi::MemRange>());
static_assert!(align_of::<Range>() == align_of::<zbi::MemRange>());
static_assert!(offset_of!(Range, addr) == offset_of!(zbi::MemRange, paddr));
static_assert!(offset_of!(Range, size) == offset_of!(zbi::MemRange, length));
static_assert!(offset_of!(Range, ty) == offset_of!(zbi::MemRange, r#type));

impl Range {
    /// Construct a new range.
    ///
    /// If `addr + size > u64::MAX`, the size is reduced so there is no overflow.
    pub const fn new(addr: u64, size: u64, ty: Type) -> Self {
        let end = addr.saturating_add(size);
        Self { addr, size: end - addr, ty }
    }

    /// Sanitize a mutable slice of [`zbi::MemRange`] in-place and view it as `&mut [Range]`.
    pub fn slice_from_zbi_mem_ranges(ranges: &mut [zbi::MemRange]) -> &mut [Self] {
        for range in ranges.iter_mut() {
            range.length = range.paddr.saturating_add(range.length) - range.paddr;
            range.reserved = 0;
        }
        // Safety:
        //
        // `Range` and `zbi::MemRange` have identical size, alignment, and field offsets
        //  (verified by `static_assert!` above).
        //
        // Every `zbi::MemType` discriminant with `reserved == 0` in the upper 32 bits is a
        // valid [`Type`].
        //
        // `paddr + length` does not overflow.
        unsafe { slice::from_raw_parts_mut(ranges.as_mut_ptr().cast::<Range>(), ranges.len()) }
    }

    /// The start address of the range.
    pub const fn addr(&self) -> u64 {
        self.addr
    }

    /// The size of the range.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The type of the range.
    pub const fn ty(&self) -> Type {
        self.ty
    }

    /// The exclusive endpoint of the range.
    pub const fn end(&self) -> u64 {
        self.addr + self.size
    }

    /// Align the ends of a range outward to be multiples of `alignment`.
    ///
    /// The start address is rounded down and the end address is rounded up.  If the end address
    /// would be larger than 2^64, we saturate it.
    ///
    /// # Panics
    ///
    /// Panics if `alignment` is not a power of two.
    pub const fn align_out(&self, alignment: u64) -> Self {
        assert!(alignment.is_power_of_two());
        let aligned_addr = self.addr & alignment.wrapping_neg();
        let aligned_end = match self.end().checked_add(alignment - 1) {
            Some(end) => end & alignment.wrapping_neg(),
            None => u64::MAX,
        };
        Self { addr: aligned_addr, size: aligned_end - aligned_addr, ty: self.ty }
    }

    /// Determine if `other` intersects with `self`.
    ///
    /// Intersection is type-agnostic.
    pub const fn intersects(&self, other: &Range) -> bool {
        self.addr < other.end() && other.addr < self.end()
    }
}

impl PartialOrd for Range {
    /// Order ranges lexicographically.
    ///
    /// Comparisons are not type sensitive unless both the start address and size are equal to each
    /// other, in which case different types yields a `None`.
    fn partial_cmp(&self, other: &Range) -> Option<Ordering> {
        match (self.addr.cmp(&other.addr), self.size.cmp(&other.size)) {
            (Ordering::Equal, Ordering::Equal) => {
                if self.ty == other.ty {
                    Some(Ordering::Equal)
                } else {
                    None
                }
            }
            (Ordering::Equal, size_ord) => Some(size_ord),
            (addr_ord, _) => Some(addr_ord),
        }
    }
}

/// An iterator over a normalized collection of ranges.
///
/// A normalized collection of ranges is one in which the ranges are:
///
/// * Lexicographically sorted on endpoint
/// * Mutually disjoint
/// * Maximally contiguous (adjacent ranges never share the same type)
///
/// # Safety
///
/// Implementors must guarantee that the items in iteration order are indeed normalized
pub unsafe trait NormalizedRangeIterator: Iterator<Item = Range> {
    /// Creates a normalized range iterator which maps and filters out the range type.
    ///
    /// `f` serves as the map that determines which types maps to which, and which are discarded
    /// or filtered out.
    fn filter_map_ranges<F>(self, f: F) -> FilterMapRanges<Self, F>
    where
        Self: Sized,
        F: Fn(Type) -> Option<Type>,
    {
        FilterMapRanges::new(self, f)
    }

    /// Creates a normalized range iterator which forgets about non-ram types, and sees allocated
    /// types as free ram.
    fn filter_map_ram(self) -> impl NormalizedRangeIterator
    where
        Self: Sized,
    {
        self.filter_map_ranges(|t| if t.is_ram() { Some(Type::FreeRam) } else { None })
    }
}

/// An iterator over a normalized collection of range which uses `f` to filter map the ranges.
///
/// The normalized invariant of the underylying collection is preserved.
pub struct FilterMapRanges<I, F> {
    ranges: I,
    f: F,
    pending: Option<Range>,
}

impl<I, F> FilterMapRanges<I, F>
where
    I: NormalizedRangeIterator,
    F: Fn(Type) -> Option<Type>,
{
    fn new(ranges: I, f: F) -> Self {
        Self { ranges, f, pending: None }
    }
}

impl<I, F> Iterator for FilterMapRanges<I, F>
where
    I: NormalizedRangeIterator,
    F: Fn(Type) -> Option<Type>,
{
    type Item = Range;

    fn next(&mut self) -> Option<Range> {
        // The strategy here is to run a sliding window [prev, cur] across the underlying ranges,
        // using `self.pending` to both store `cur` and collect adjacent ranges of the same type.
        // A tail-recursive call indicates we slide the window by one without yielding anything.

        let Some(cur) = self.ranges.next() else {
            // Our sliding window has reached the end.
            return self.pending.take();
        };

        // Send cur to f(cur).
        let Some(ty) = (self.f)(cur.ty) else {
            // The `f` map discards this range.
            return self.next();
        };

        let cur = Range { addr: cur.addr, size: cur.size, ty };

        let Some(prev) = self.pending.take() else {
            // This is the first call to `next()`.
            self.pending = Some(cur);
            return self.next();
        };

        if prev.end() == cur.addr && prev.ty == cur.ty {
            self.pending = Some(Range::new(prev.addr, prev.size + cur.size, prev.ty));
            return self.next();
        }

        self.pending = Some(cur);
        Some(prev)
    }
}

/// Safety: Merging of neighboring ranges while tweaking types preserves normalization.
unsafe impl<I, F> NormalizedRangeIterator for FilterMapRanges<I, F>
where
    I: NormalizedRangeIterator,
    F: Fn(Type) -> Option<Type>,
{
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    // [ Book ][ Zbi ][ Test ][Peri ]     [ Test ][Kern ]
    const TEST_RANGES: [Range; 6] = [
        Range::new(0, 0x1000, Type::PoolBookkeeping),
        Range::new(0x1000, 0x1000, Type::DataZbi),
        Range::new(0x2000, 0x1000, Type::PoolTestPayload),
        Range::new(0x3000, 0x1000, Type::Peripheral),
        Range::new(0x5000, 0x1000, Type::PoolTestPayload),
        Range::new(0x6000, 0x1000, Type::PhysKernel),
    ];

    // Non-overflowing range preserves addr and size.
    #[test]
    fn range_test_new() {
        assert_eq!(
            Range::new(0, 0x1000, Type::FreeRam),
            Range { addr: 0, size: 0x1000, ty: Type::FreeRam },
        );
    }

    // Overflowing `addr + size` saturates at `u64::MAX`.
    #[test]
    fn range_test_new_saturates() {
        assert_eq!(
            Range::new(u64::MAX - 1, 0x1000, Type::FreeRam),
            Range::new(u64::MAX - 1, 1, Type::FreeRam),
        );
        assert_eq!(Range::new(u64::MAX, 1, Type::FreeRam), Range::new(u64::MAX, 0, Type::FreeRam));
    }

    // Unaligned start rounds down to alignment boundary.
    #[test]
    fn range_test_align_out_start_rounds_down() {
        let r = Range::new(1, 0x1000 - 1, Type::FreeRam);
        assert_eq!(r.align_out(0x1000), Range::new(0, 0x1000, Type::FreeRam));
    }

    // Unaligned end rounds up to alignment boundary.
    #[test]
    fn range_test_align_out_end_rounds_up() {
        let r = Range::new(0, 0x1000 - 1, Type::FreeRam);
        assert_eq!(r.align_out(0x1000), Range::new(0, 0x1000, Type::FreeRam));
    }

    // Sub-chunk range expands to a full aligned chunk.
    #[test]
    fn range_test_align_out_range_smaller_than_alignment() {
        let r = Range::new(1, 1, Type::FreeRam);
        assert_eq!(r.align_out(0x1000), Range::new(0, 0x1000, Type::FreeRam));
    }

    // Unaligned start and end round outward in both directions.
    #[test]
    fn range_test_align_out_both_ends_change() {
        let r = Range::new(1, 0x1000, Type::FreeRam);
        assert_eq!(r.align_out(0x1000), Range::new(0, 2 * 0x1000, Type::FreeRam));
    }

    // Upward end alignment saturates at `u64::MAX` on overflow.
    #[test]
    fn range_test_align_out_saturates() {
        let r = Range::new(u64::MAX - 1, 1, Type::FreeRam);
        assert_eq!(
            r.align_out(0x1000),
            Range::new(u64::MAX - 0x1000 + 1, 0x1000 - 1, Type::FreeRam),
        );
    }

    // Empty ranges are still aligned outward to have a size of `alignment`.
    #[test]
    fn range_test_align_out_empty_range() {
        let r = Range::new(0x3000 + 1, 0, Type::FreeRam);
        assert_eq!(r.align_out(0x1000), Range::new(0x3000, 0x1000, Type::FreeRam));
    }

    // Adjacent ranges do not intersect.
    //
    // Input:
    // [          )
    //            [          )
    // Output: false
    #[test]
    fn range_test_intersects_disjoint() {
        let a = Range::new(0, 0x1000, Type::FreeRam);
        let b = Range::new(0x1000, 0x1000, Type::FreeRam);
        assert!(!a.intersects(&b));
        assert!(!b.intersects(&a));
    }

    // A non-empty range intersects with itself.
    //
    // Input:
    // [          )
    // [          )
    // Output: true
    #[test]
    fn range_test_intersects_identity() {
        let r = Range::new(0, 0x1000, Type::FreeRam);
        assert!(r.intersects(&r));
    }

    // A range strictly containing another intersects with it.
    //
    // Input:
    // [                                    )
    //            [            )
    // Output: true
    #[test]
    fn range_test_intersects_eclipse() {
        let outer = Range::new(0, 0x3000, Type::FreeRam);
        let inner = Range::new(0x1000, 0x1000, Type::FreeRam);
        assert!(outer.intersects(&inner));
        assert!(inner.intersects(&outer));
    }

    // Ranges overlapping on the right side intersect.
    //
    // Input:
    //        [                       )
    //                 [                         )
    // Output: true
    #[test]
    fn range_test_intersects_clip_right() {
        let r = Range::new(0, 0x2000, Type::FreeRam);
        let other = Range::new(0x1000, 0x2000, Type::FreeRam);
        assert!(r.intersects(&other));
    }

    // Ranges overlapping on the left side intersect.
    //
    // Input:
    //                   [                         )
    //       [                       )
    // Output: true
    #[test]
    fn range_test_intersects_clip_left() {
        let r = Range::new(0x1000, 0x2000, Type::FreeRam);
        let other = Range::new(0, 0x2000, Type::FreeRam);
        assert!(r.intersects(&other));
    }

    // Overlapping ranges of different types still intersect.
    //
    // Input:
    //              [          FreeRam         )
    //                         [      Peripheral        )
    // Output: true
    #[test]
    fn range_test_intersects_type_agnostic() {
        let a = Range::new(0, 0x2000, Type::FreeRam);
        let b = Range::new(0x1000, 0x2000, Type::Peripheral);
        assert!(a.intersects(&b));
    }

    // Empty ranges will not intersect with other empty ranges at the same address.
    //
    // Input:
    //              [)
    //              [)
    // Output: false
    #[test]
    fn range_test_intersects_empty_range_with_other_zero_sized_range() {
        let a = Range::new(0x3000, 0, Type::FreeRam);
        let b = Range::new(0x3000, 0, Type::Peripheral);
        assert!(!a.intersects(&b));
    }

    // An empty range will not intersect with non-empty ranges at the same address.
    //
    // Input:
    //              [)
    //              [      Peripheral        )
    // Output: false
    #[test]
    fn range_test_intersects_empty_range_at_same_start_address() {
        let a = Range::new(0x1000, 0, Type::FreeRam);
        let b = Range::new(0x1000, 0x2000, Type::Peripheral);
        assert!(!a.intersects(&b));
    }

    // An empty range will intersect with an eclipsing range.
    //
    // Input:
    //                      [)
    //          [      Peripheral        )
    // Output: true
    #[test]
    fn range_test_intersects_type_empty_range_eclipsed() {
        let a = Range::new(0x3000, 0, Type::FreeRam);
        let b = Range::new(0x1000, 0x10000, Type::Peripheral);
        assert!(a.intersects(&b));
    }

    // Lower start address orders before higher start address regardless of type.
    //
    // a: [ FreeRam  )
    // b:            [Peripheral)
    //
    // a < b
    #[test]
    fn range_test_partial_cmp_addr_less_than() {
        let a = Range::new(0, 0x1000, Type::FreeRam);
        let b = Range::new(0x1000, 0x1000, Type::Peripheral);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Less));
        assert_eq!(b.partial_cmp(&a), Some(Ordering::Greater));
    }

    // Equal start addresses order by size regardless of type.
    //
    // a: [ FreeRam  )
    // b: [     Peripheral      )
    //
    // a < b
    #[test]
    fn range_test_partial_cmp_smaller() {
        let a = Range::new(0, 0x1000, Type::FreeRam);
        let b = Range::new(0, 0x2000, Type::Peripheral);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Less));
        assert_eq!(b.partial_cmp(&a), Some(Ordering::Greater));
    }

    // Identical bounds compare `Equal` if types match, or `None` if types differ.
    //
    // a: [   FreeRam   )
    // b: [   FreeRam   )
    // c: [  Peripheral )
    //
    // a == b
    // a == c is undefined.
    #[test]
    fn range_test_partial_cmp_eq() {
        let a = Range::new(0, 0x1000, Type::FreeRam);
        let b = Range::new(0, 0x1000, Type::FreeRam);
        let c = Range::new(0, 0x1000, Type::Peripheral);
        assert_eq!(a.partial_cmp(&b), Some(Ordering::Equal));
        assert_eq!(a.partial_cmp(&c), None);
    }

    // [`Range::slice_from_zbi_mem_ranges`] zeroes `reserved`, saturates overflowing lengths, and
    // casts in-place.
    #[test]
    fn range_test_from_zbi_ranges() {
        fn check_ranges(ranges: &[Range]) {
            assert_eq!(
                ranges,
                &[
                    Range::new(0, 0x1000, Type::FreeRam),
                    Range::new(0x1000, 0x1000, Type::Peripheral),
                    Range::new(u64::MAX - 1, 1, Type::Reserved),
                ]
            );
        }

        let mut zbi_ranges = [
            zbi::MemRange {
                paddr: 0,
                length: 0x1000,
                r#type: zbi::MemType::Ram,
                reserved: 0xdeadbeef,
            },
            zbi::MemRange {
                paddr: 0x1000,
                length: 0x1000,
                r#type: zbi::MemType::Peripheral,
                reserved: 0x12345678,
            },
            zbi::MemRange {
                paddr: u64::MAX - 1,
                length: 0x1000,
                r#type: zbi::MemType::Reserved,
                reserved: u32::MAX,
            },
        ];

        check_ranges(Range::slice_from_zbi_mem_ranges(&mut zbi_ranges));
    }

    // Coalesces contiguous RAM ranges into `FreeRam` and drops non-ram ranges.
    //
    // Input:  [ Book ][ Zbi ][ Test ][Peri ]     [ Test ][Kern ]
    // Output: [      FreeRam        ]            [   FreeRam   ]
    #[test]
    fn normalized_range_iterator_test_filter_map_ram() {
        let mut actual =
            testing::NormalizedRangeCheckingIterator::new(TEST_RANGES).filter_map_ram();
        assert_eq!(actual.next(), Some(Range::new(0, 0x3000, Type::FreeRam)));
        assert_eq!(actual.next(), Some(Range::new(0x5000, 0x2000, Type::FreeRam)));
        assert_eq!(actual.next(), None);
    }

    // Mapping all types to `None` yields an empty iterator.
    //
    // Input:  [ Book ][ Zbi ][ Test ][Peri ]     [ Test ][Kern ]
    // Output:
    #[test]
    fn normalized_range_iterator_test_filter_map_ranges_discard_all() {
        let mut actual =
            testing::NormalizedRangeCheckingIterator::new(TEST_RANGES).filter_map_ranges(|_| None);
        assert_eq!(actual.next(), None);
    }

    // Discarding RAM ranges leaves only `Peripheral`.
    //
    // Input:  [ Book ][ Zbi ][ Test ][Peri ]     [ Test ][Kern ]
    // Output:                        [Peri ]
    #[test]
    fn normalized_range_iterator_test_filter_map_ranges_discard_ram() {
        let mut actual = testing::NormalizedRangeCheckingIterator::new(TEST_RANGES)
            .filter_map_ranges(|r| if r.is_ram() { None } else { Some(r) });
        assert_eq!(actual.next(), Some(Range::new(0x3000, 0x1000, Type::Peripheral)));
        assert_eq!(actual.next(), None);
    }

    // Retains only `PoolBookkeeping` and `PoolTestPayload` ranges.
    //
    // Input:  [ Book ][ Zbi ][ Test ][Peri ]     [ Test ][Kern ]
    // Output: [ Book ]       [ Test ]            [ Test ]
    #[test]
    fn normalized_range_iterator_test_filter_map_ranges_keep_pool_test_payloads_and_bookkeeping() {
        let mut actual = testing::NormalizedRangeCheckingIterator::new(TEST_RANGES)
            .filter_map_ranges(|r| {
                if r == Type::PoolBookkeeping || r == Type::PoolTestPayload {
                    Some(r)
                } else {
                    None
                }
            });
        assert_eq!(actual.next(), Some(Range::new(0, 0x1000, Type::PoolBookkeeping)));
        assert_eq!(actual.next(), Some(Range::new(0x2000, 0x1000, Type::PoolTestPayload)));
        assert_eq!(actual.next(), Some(Range::new(0x5000, 0x1000, Type::PoolTestPayload)));
        assert_eq!(actual.next(), None);
    }
}
