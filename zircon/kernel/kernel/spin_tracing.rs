// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel lock spin tracing and lock name storage.

use core::ffi::c_void;
use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ktrace_rs::{
    ArgValue, Argument, Context, EventType, InstantBootTicks, InternedString, KTrace,
    resolve_category, resolve_string,
};

pub const SCHEDULER_LOCK_SPIN_TRACING_ENABLED: bool = cfg!(scheduler_lock_spin_tracing_enabled);
pub const SCHEDULER_LOCK_SPIN_TRACING_COMPRESSED: bool =
    cfg!(scheduler_lock_spin_tracing_compressed);
pub const LOCK_NAME_TRACING_ENABLED: bool = cfg!(lock_name_tracing_enabled);

pub const SPINLOCK_NAME_TRACING_ENABLED: bool = SCHEDULER_LOCK_SPIN_TRACING_ENABLED;
pub const MUTEX_NAME_TRACING_ENABLED: bool =
    SCHEDULER_LOCK_SPIN_TRACING_ENABLED || LOCK_NAME_TRACING_ENABLED;
pub const RWLOCK_NAME_TRACING_ENABLED: bool =
    SCHEDULER_LOCK_SPIN_TRACING_ENABLED || LOCK_NAME_TRACING_ENABLED;

// Compress lock-spin trace records requires lock-spin tracing to be enabled.
zr::static_assert!(!SCHEDULER_LOCK_SPIN_TRACING_COMPRESSED || SCHEDULER_LOCK_SPIN_TRACING_ENABLED);

mod internal {
    /// A bitfield helper for packing and unpacking integers within a 64-bit word.
    pub struct Bitfield<const BITS: u32, const SHIFT: u32>;

    impl<const BITS: u32, const SHIFT: u32> Bitfield<BITS, SHIFT> {
        /// Bitmask of width `BITS` positioned at bit 0.
        pub const MASK: u64 = if BITS >= 64 { u64::MAX } else { (1u64 << BITS) - 1 };

        /// Masks `val` to `BITS` bits and shifts it left by `SHIFT`.
        #[inline(always)]
        pub const fn encode(val: u64) -> u64 {
            (val & Self::MASK) << SHIFT
        }

        /// Extracts `BITS` bits starting at bit `SHIFT` from `val`.
        #[inline(always)]
        pub const fn decode(val: u64) -> u64 {
            (val >> SHIFT) & Self::MASK
        }

        /// Replaces the `BITS`-wide field at `SHIFT` in `old_val` with `new_val`.
        #[inline(always)]
        pub const fn reset(old_val: u64, new_val: u64) -> u64 {
            (old_val & !(Self::MASK << SHIFT)) | ((new_val & Self::MASK) << SHIFT)
        }
    }
}

/// The type of lock being traced.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockType {
    Spinlock = 0,
    Mutex = 1,
    ChainLock = 2,
    RwLock = 3,
}

pub const LOCK_TYPE_SPINLOCK: u8 = LockType::Spinlock as u8;
pub const LOCK_TYPE_MUTEX: u8 = LockType::Mutex as u8;
pub const LOCK_TYPE_CHAIN_LOCK: u8 = LockType::ChainLock as u8;
pub const LOCK_TYPE_RW_LOCK: u8 = LockType::RwLock as u8;

impl LockType {
    /// Converts a raw `u8` discriminant into a [`LockType`].
    ///
    /// # Panics
    ///
    /// Panics if `val` does not correspond to a valid `LockType` discriminant (`0..=3`).
    #[inline(always)]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            LOCK_TYPE_SPINLOCK => LockType::Spinlock,
            LOCK_TYPE_MUTEX => LockType::Mutex,
            LOCK_TYPE_CHAIN_LOCK => LockType::ChainLock,
            LOCK_TYPE_RW_LOCK => LockType::RwLock,
            _ => panic!("invalid LockType discriminant"),
        }
    }
}

/// The outcome of the lock acquisition attempt.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FinishType {
    LockAcquired = 0,
    Blocked = 1,
}

pub const FINISH_TYPE_LOCK_ACQUIRED: u8 = FinishType::LockAcquired as u8;
pub const FINISH_TYPE_BLOCKED: u8 = FinishType::Blocked as u8;

impl FinishType {
    /// Converts a raw `u8` discriminant into a [`FinishType`].
    ///
    /// # Panics
    ///
    /// Panics if `val` does not correspond to a valid `FinishType` discriminant (`0..=1`).
    #[inline(always)]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            FINISH_TYPE_LOCK_ACQUIRED => FinishType::LockAcquired,
            FINISH_TYPE_BLOCKED => FinishType::Blocked,
            _ => panic!("invalid FinishType discriminant"),
        }
    }
}

/// A compact 64-bit encoded lock identifier.
///
/// Bit layout:
/// - `[0..48]` (49 bits): Unique lock ID.
/// - `[49..60]` (12 bits): Lock class name ID.
/// - `[61..62]` (2 bits): Lock type (`LockType`).
/// - `[63..63]` (1 bit): Finish type (`FinishType`).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EncodedLockId {
    value: u64,
}

type IdBits = internal::Bitfield<49, 0>;
type ClassNameBits = internal::Bitfield<12, 49>;
type LockTypeBits = internal::Bitfield<2, 61>;
type FinishTypeBits = internal::Bitfield<1, 63>;

impl EncodedLockId {
    pub const INVALID: Self = Self { value: 0 };

    /// Creates a new `EncodedLockId` with the given lock type, lock ID, and class name ID.
    #[inline(always)]
    pub const fn new(lock_type: LockType, lock_id: u64, class_name_id: u16) -> Self {
        Self {
            value: LockTypeBits::encode(lock_type as u64)
                | IdBits::encode(lock_id)
                | ClassNameBits::encode(class_name_id as u64),
        }
    }

    /// Creates an invalid `EncodedLockId` (all zeros).
    #[inline(always)]
    pub const fn invalid() -> Self {
        Self::INVALID
    }

    /// Creates an `EncodedLockId` directly from a raw 64-bit value.
    #[inline(always)]
    pub const fn from_raw(value: u64) -> Self {
        Self { value }
    }

    /// Updates the lock class ID.
    #[inline(always)]
    pub const fn set_lock_class_id(&mut self, class_name_id: u16) {
        self.value = ClassNameBits::reset(self.value, class_name_id as u64);
    }

    /// Returns a new `EncodedLockId` with the lock class ID updated.
    #[inline(always)]
    pub const fn with_lock_class_id(mut self, class_name_id: u16) -> Self {
        self.set_lock_class_id(class_name_id);
        self
    }

    /// Encodes the finish type into bit 63 and returns the raw 64-bit integer.
    #[inline(always)]
    pub const fn finished_value(&self, finish_type: FinishType) -> u64 {
        self.value | FinishTypeBits::encode(finish_type as u64)
    }

    /// Returns the underlying raw 64-bit value.
    #[inline(always)]
    pub const fn value(&self) -> u64 {
        self.value
    }

    /// Returns the 49-bit lock ID.
    #[inline(always)]
    pub const fn id(&self) -> u64 {
        IdBits::decode(self.value)
    }

    /// Returns the 12-bit lock class name ID.
    #[inline(always)]
    pub const fn class_name(&self) -> u16 {
        ClassNameBits::decode(self.value) as u16
    }

    /// Returns the 2-bit lock type.
    #[inline(always)]
    pub const fn lock_type(&self) -> LockType {
        LockType::from_u8(LockTypeBits::decode(self.value) as u8)
    }

    /// Returns the 1-bit finish type.
    #[inline(always)]
    pub const fn finish_type(&self) -> FinishType {
        FinishType::from_u8(FinishTypeBits::decode(self.value) as u8)
    }
}

impl From<u64> for EncodedLockId {
    #[inline(always)]
    fn from(value: u64) -> Self {
        Self::from_raw(value)
    }
}

impl From<EncodedLockId> for u64 {
    #[inline(always)]
    fn from(elid: EncodedLockId) -> Self {
        elid.value()
    }
}

zr::static_assert!(size_of::<EncodedLockId>() == 8);
zr::static_assert!(align_of::<EncodedLockId>() == 8);

/// Monotonically increasing lock ID generator.
pub struct LockIdGenerator;

static LOCK_ID_GENERATOR: AtomicU64 = AtomicU64::new(1);

impl LockIdGenerator {
    /// Generates a globally unique 64-bit lock ID with relaxed ordering.
    #[inline]
    pub fn create_id() -> u64 {
        LOCK_ID_GENERATOR.fetch_add(1, Ordering::Relaxed)
    }
}

/// FFI entry point so C++ `spin_tracing::LockIdGenerator::CreateId()` shares the same atomic
/// counter as Rust locks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_spin_tracing_create_lock_id() -> u64 {
    LockIdGenerator::create_id()
}

/// Represents an FXT string reference by numeric ID.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdStringRef(u16);

impl IdStringRef {
    pub const INVALID_ID: u16 = 0;

    /// Creates a new `IdStringRef` wrapping the given 16-bit string table ID.
    #[inline(always)]
    pub const fn new(id: u16) -> Self {
        Self(id)
    }

    /// Returns the numeric 16-bit string table ID.
    #[inline(always)]
    pub const fn id(&self) -> u16 {
        self.0
    }

    /// Returns the string reference for disabled tracing (`"[disabled]"`).
    #[inline]
    pub fn disabled() -> Self {
        Self(resolve_string!("[disabled]").id())
    }

    /// Returns the string reference for an invalid/unnamed lock (`"[invalid]"`).
    #[inline]
    pub fn invalid() -> Self {
        Self(resolve_string!("[invalid]").id())
    }
}

impl From<u16> for IdStringRef {
    #[inline(always)]
    fn from(id: u16) -> Self {
        Self::new(id)
    }
}

impl From<IdStringRef> for u16 {
    #[inline(always)]
    fn from(r: IdStringRef) -> Self {
        r.id()
    }
}

impl From<&'static InternedString> for IdStringRef {
    #[inline]
    fn from(s: &'static InternedString) -> Self {
        Self::new(s.id())
    }
}

impl<'a> From<IdStringRef> for ArgValue<'a> {
    #[inline(always)]
    fn from(r: IdStringRef) -> Self {
        ArgValue::StringId(r.id())
    }
}

#[doc(hidden)]
pub trait StorageSelector<const ENABLED: bool> {
    type Data: Copy + Clone + core::fmt::Debug + PartialEq + Eq;
}

#[doc(hidden)]
pub trait TimestampSelector<const ENABLED: bool> {
    type Data: Copy + Clone + core::fmt::Debug + PartialEq + Eq;
}

#[doc(hidden)]
pub struct BoolSelector<const ENABLED: bool>;

impl StorageSelector<true> for BoolSelector<true> {
    type Data = EncodedLockId;
}

impl StorageSelector<false> for BoolSelector<false> {
    type Data = ();
}

impl TimestampSelector<true> for BoolSelector<true> {
    type Data = InstantBootTicks;
}

impl TimestampSelector<false> for BoolSelector<false> {
    type Data = ();
}

/// Operations supported by `LockNameStorage` regardless of whether tracing is enabled or disabled.
pub trait LockNameStorageOps {
    /// Sets the lock class name from a raw `lockdep::LockClassId` pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either `lcid` is null, or all of the following hold:
    /// 1. For the duration of this call, `lcid` is properly aligned for `*const InternedString`,
    ///    valid for reads of `size_of::<*const InternedString>()` bytes, points to an initialized
    ///    `*const InternedString` (such as the first field of a live `MetadataLockClassState` or
    ///    `ValidatorLockClassState`), and is not concurrently mutated.
    /// 2. The `*const InternedString` value stored at `lcid` is either null or properly aligned
    ///    for `InternedString` and points to an initialized, immutable `'static` `InternedString`
    ///    allocated within the `__fxt_interned_string_table` linker section.
    unsafe fn set_lock_class_id(&mut self, lcid: *const c_void);

    /// Configures the lock class name from a type implementing `lockdep::LockClass`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`Self::set_lock_class_id`].
    unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self);

    /// Returns the encoded lock ID.
    fn encoded_lock_id(&self) -> EncodedLockId;

    /// Returns the string reference for this lock class name.
    fn class_name_ref(&self) -> IdStringRef;

    /// Assigns a new unique lock ID if currently zero (e.g. after `const_init`).
    fn ensure_id(&mut self);
}

/// Storage for lock name tracing and encoded lock identification.
///
/// When `ENABLED` is false, this struct is zero-sized and operations compile to zero-cost no-ops.
/// When `ENABLED` is true, this struct stores an 8-byte `EncodedLockId` and generates a unique
/// lock ID.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LockNameStorage<const LOCK_TYPE: u8, const ENABLED: bool>
where
    BoolSelector<ENABLED>: StorageSelector<ENABLED>,
{
    data: <BoolSelector<ENABLED> as StorageSelector<ENABLED>>::Data,
}

impl<const LOCK_TYPE: u8, const ENABLED: bool> PartialEq for LockNameStorage<LOCK_TYPE, ENABLED>
where
    BoolSelector<ENABLED>: StorageSelector<ENABLED>,
{
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<const LOCK_TYPE: u8, const ENABLED: bool> Eq for LockNameStorage<LOCK_TYPE, ENABLED> where
    BoolSelector<ENABLED>: StorageSelector<ENABLED>
{
}

// Specialization for `ENABLED = false`
impl<const LOCK_TYPE: u8> LockNameStorage<LOCK_TYPE, false> {
    /// Creates a disabled `LockNameStorage`.
    #[inline(always)]
    pub fn new() -> Self {
        const {
            let _ = LockType::from_u8(LOCK_TYPE);
        };
        Self { data: () }
    }

    /// Creates a disabled `LockNameStorage` in a const context for two-phase initialization
    /// before the storage is shared.
    #[inline(always)]
    pub const fn const_init(_class_name_id: u16) -> Self {
        const {
            let _ = LockType::from_u8(LOCK_TYPE);
        };
        Self { data: () }
    }

    /// Creates a disabled `LockNameStorage` ignoring the class name ID.
    #[inline(always)]
    pub fn with_class_name_id(_class_name_id: u16) -> Self {
        const {
            let _ = LockType::from_u8(LOCK_TYPE);
        };
        Self { data: () }
    }

    /// Creates a disabled `LockNameStorage` ignoring the interned string.
    #[inline(always)]
    pub fn with_class_name(_name: &'static InternedString) -> Self {
        const {
            let _ = LockType::from_u8(LOCK_TYPE);
        };
        Self { data: () }
    }

    /// No-op when lock name tracing is disabled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either `lcid` is null, or all of the following hold:
    /// 1. For the duration of this call, `lcid` is properly aligned for `*const InternedString`,
    ///    valid for reads of `size_of::<*const InternedString>()` bytes, points to an initialized
    ///    `*const InternedString` (such as the first field of a live `MetadataLockClassState` or
    ///    `ValidatorLockClassState`), and is not concurrently mutated.
    /// 2. The `*const InternedString` value stored at `lcid` is either null or properly aligned
    ///    for `InternedString` and points to an initialized, immutable `'static` `InternedString`
    ///    allocated within the `__fxt_interned_string_table` linker section.
    #[inline(always)]
    pub unsafe fn set_lock_class_id(&mut self, _lcid: *const c_void) {}

    /// No-op when lock name tracing is disabled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`Self::set_lock_class_id`].
    #[inline(always)]
    pub unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self) {}

    /// No-op when lock name tracing is disabled.
    #[inline(always)]
    pub const fn set_class_name_id(&mut self, _class_name_id: u16) {}

    /// No-op when lock name tracing is disabled.
    #[inline(always)]
    pub fn set_class_name(&mut self, _name: &'static InternedString) {}

    /// Returns `EncodedLockId::INVALID`.
    #[inline(always)]
    pub const fn encoded_lock_id(&self) -> EncodedLockId {
        EncodedLockId::INVALID
    }

    /// Returns the string reference for `"[disabled]"`.
    #[inline]
    pub fn class_name_ref(&self) -> IdStringRef {
        IdStringRef::disabled()
    }

    /// No-op when lock name tracing is disabled.
    #[inline(always)]
    pub fn ensure_id(&mut self) {}
}

impl<const LOCK_TYPE: u8> Default for LockNameStorage<LOCK_TYPE, false> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl<const LOCK_TYPE: u8> LockNameStorageOps for LockNameStorage<LOCK_TYPE, false> {
    /// # Safety
    ///
    /// The caller must ensure that `lcid` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline(always)]
    unsafe fn set_lock_class_id(&mut self, _lcid: *const c_void) {}

    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline(always)]
    unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self) {}

    #[inline(always)]
    fn encoded_lock_id(&self) -> EncodedLockId {
        EncodedLockId::INVALID
    }

    #[inline]
    fn class_name_ref(&self) -> IdStringRef {
        IdStringRef::disabled()
    }

    #[inline(always)]
    fn ensure_id(&mut self) {}
}

// Specialization for `ENABLED = true`
impl<const LOCK_TYPE: u8> LockNameStorage<LOCK_TYPE, true> {
    /// Constructs an enabled `LockNameStorage` assigning a new unique ID and an invalid class
    /// name ID.
    #[inline]
    pub fn new() -> Self {
        let lock_type = const { LockType::from_u8(LOCK_TYPE) };
        let id = LockIdGenerator::create_id();
        Self { data: EncodedLockId::new(lock_type, id, IdStringRef::INVALID_ID) }
    }

    /// Statically constructs a `LockNameStorage` in a `const` context with a placeholder ID (0)
    /// for two-phase initialization (via [`Self::ensure_id`]) before the storage is shared.
    #[inline(always)]
    pub const fn const_init(class_name_id: u16) -> Self {
        let lock_type = const { LockType::from_u8(LOCK_TYPE) };
        Self { data: EncodedLockId::new(lock_type, 0, class_name_id) }
    }

    /// Constructs an enabled `LockNameStorage` assigning a new unique ID and the given class
    /// name ID.
    #[inline]
    pub fn with_class_name_id(class_name_id: u16) -> Self {
        let lock_type = const { LockType::from_u8(LOCK_TYPE) };
        let id = LockIdGenerator::create_id();
        Self { data: EncodedLockId::new(lock_type, id, class_name_id) }
    }

    /// Constructs an enabled `LockNameStorage` assigning a new unique ID and the given interned
    /// string name.
    #[inline]
    pub fn with_class_name(name: &'static InternedString) -> Self {
        Self::with_class_name_id(name.id())
    }

    /// Looks up the lock class name ID from the lock class metadata and stores it.
    ///
    /// If `lcid` is null or the lock class name has already been set, this is a no-op.
    ///
    /// # Safety
    ///
    /// The caller must ensure that either `lcid` is null, or all of the following hold:
    /// 1. For the duration of this call, `lcid` is properly aligned for `*const InternedString`,
    ///    valid for reads of `size_of::<*const InternedString>()` bytes, points to an initialized
    ///    `*const InternedString` (such as the first field of a live `MetadataLockClassState` or
    ///    `ValidatorLockClassState`), and is not concurrently mutated.
    /// 2. The `*const InternedString` value stored at `lcid` is either null or properly aligned
    ///    for `InternedString` and points to an initialized, immutable `'static` `InternedString`
    ///    allocated within the `__fxt_interned_string_table` linker section.
    pub unsafe fn set_lock_class_id(&mut self, lcid: *const c_void) {
        if !lcid.is_null() && self.data.class_name() == IdStringRef::INVALID_ID {
            // SAFETY:
            // - `!lcid.is_null()` was checked in the enclosing `if` condition and `lcid` is an
            //   unmutated local copy.
            // - By this function's `# Safety` precondition 1, `lcid` is properly aligned for
            //   `*const InternedString`, valid for reads of `size_of::<*const InternedString>()`
            //   bytes for the duration of this call, initialized to a `*const InternedString`,
            //   and not concurrently mutated.
            let string_ptr = unsafe { *lcid.cast::<*const InternedString>() };

            // Note: in a perfect world it should be absolutely impossible for the
            // pointer stored in `MetadataLockClassState` to be null. Unfortunately,
            // because of _technically_ possible global ctor races, it is possible to
            // be accessing a LockClassState which has not fully been constructed yet,
            // meaning that its reference to its name might be null (something which
            // _should_ be impossible). Until all lock class states are const-initialized,
            // there is always a small chance that this could happen in an instrumented
            // build.
            //
            // If/when it does happen, it can be a pretty unpleasant experience as the
            // kernel will panic before there is even serial enabled, making things
            // extremely difficult to debug. Adding this check makes for a better
            // experience in the worst case scenario. It is possible for a lock to go
            // unnamed, but at least we won't crash during early boot.
            if !string_ptr.is_null() {
                // SAFETY:
                // - `!string_ptr.is_null()` was checked in the enclosing `if` condition and
                //   `string_ptr` is an unmutated local copy.
                // - By this function's `# Safety` precondition 2, a non-null `string_ptr` loaded
                //   from `lcid` is properly aligned for `InternedString` and points to an
                //   initialized, immutable `'static` `InternedString` in the
                //   `__fxt_interned_string_table` linker section.
                let interned = unsafe { &*string_ptr };
                let id = interned.id();
                self.data.set_lock_class_id(id);
            }
        }
    }

    /// Configures the lock class name from a type implementing `lockdep::LockClass`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`Self::set_lock_class_id`].
    #[inline]
    pub unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self) {
        // SAFETY: `self.set_lock_class_id` requires `C::ID` to satisfy its pointer preconditions,
        // which the caller guarantees under this method's `# Safety` contract, and `C::ID` is
        // forwarded unchanged.
        unsafe { self.set_lock_class_id(C::ID) };
    }

    /// Sets the lock class name ID directly.
    #[inline(always)]
    pub const fn set_class_name_id(&mut self, class_name_id: u16) {
        self.data.set_lock_class_id(class_name_id);
    }

    /// Sets the lock class name directly from an `InternedString`.
    #[inline]
    pub fn set_class_name(&mut self, name: &'static InternedString) {
        self.data.set_lock_class_id(name.id());
    }

    /// Returns the encoded lock ID.
    #[inline(always)]
    pub const fn encoded_lock_id(&self) -> EncodedLockId {
        self.data
    }

    /// Returns the string reference for this lock class name, or `"[invalid]"` if not set.
    #[inline]
    pub fn class_name_ref(&self) -> IdStringRef {
        let id = self.data.class_name();
        if id == IdStringRef::INVALID_ID { IdStringRef::invalid() } else { IdStringRef::new(id) }
    }

    /// Assigns a new unique lock ID from `LockIdGenerator` if currently zero (e.g. after
    /// `const_init`).
    pub fn ensure_id(&mut self) {
        if self.data.id() == 0 {
            let new_id = LockIdGenerator::create_id();
            self.data = EncodedLockId::new(self.data.lock_type(), new_id, self.data.class_name());
        }
    }
}

impl<const LOCK_TYPE: u8> Default for LockNameStorage<LOCK_TYPE, true> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const LOCK_TYPE: u8> LockNameStorageOps for LockNameStorage<LOCK_TYPE, true> {
    /// # Safety
    ///
    /// The caller must ensure that `lcid` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline]
    unsafe fn set_lock_class_id(&mut self, lcid: *const c_void) {
        // SAFETY: `LockNameStorage::<LOCK_TYPE, true>::set_lock_class_id` has the identical
        // safety contract on `lcid` as `LockNameStorageOps::set_lock_class_id`, which the caller
        // guarantees, and `lcid` is forwarded unchanged.
        unsafe { self.set_lock_class_id(lcid) };
    }

    /// # Safety
    ///
    /// The caller must ensure that `C::ID` satisfies all preconditions of
    /// [`LockNameStorageOps::set_lock_class_id`].
    #[inline]
    unsafe fn set_lock_class<C: lockdep::LockClass>(&mut self) {
        // SAFETY: `LockNameStorage::<LOCK_TYPE, true>::set_lock_class` has the identical
        // safety contract on `C::ID` as `LockNameStorageOps::set_lock_class`, which the caller
        // guarantees.
        unsafe { self.set_lock_class::<C>() };
    }

    #[inline(always)]
    fn encoded_lock_id(&self) -> EncodedLockId {
        self.encoded_lock_id()
    }

    #[inline]
    fn class_name_ref(&self) -> IdStringRef {
        self.class_name_ref()
    }

    #[inline]
    fn ensure_id(&mut self) {
        self.ensure_id();
    }
}

pub type SpinLockNameStorage<const ENABLED: bool = SPINLOCK_NAME_TRACING_ENABLED> =
    LockNameStorage<LOCK_TYPE_SPINLOCK, ENABLED>;

pub type MutexNameStorage<const ENABLED: bool = MUTEX_NAME_TRACING_ENABLED> =
    LockNameStorage<LOCK_TYPE_MUTEX, ENABLED>;

pub type ChainLockNameStorage<const ENABLED: bool = SCHEDULER_LOCK_SPIN_TRACING_ENABLED> =
    LockNameStorage<LOCK_TYPE_CHAIN_LOCK, ENABLED>;

pub type RwLockNameStorage<const ENABLED: bool = RWLOCK_NAME_TRACING_ENABLED> =
    LockNameStorage<LOCK_TYPE_RW_LOCK, ENABLED>;

zr::static_assert!(size_of::<LockNameStorage<0, false>>() == 0);
zr::static_assert!(align_of::<LockNameStorage<0, false>>() == 1);
zr::static_assert!(size_of::<LockNameStorage<0, true>>() == 8);
zr::static_assert!(align_of::<LockNameStorage<0, true>>() == 8);

/// Captures a timestamp for spin tracing when `ENABLED` is true, or acts as a zero-sized no-op
/// when `ENABLED` is false.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpinTracingTimestamp<const ENABLED: bool = SCHEDULER_LOCK_SPIN_TRACING_ENABLED>
where
    BoolSelector<ENABLED>: TimestampSelector<ENABLED>,
{
    data: <BoolSelector<ENABLED> as TimestampSelector<ENABLED>>::Data,
}

impl<const ENABLED: bool> PartialEq for SpinTracingTimestamp<ENABLED>
where
    BoolSelector<ENABLED>: TimestampSelector<ENABLED>,
{
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<const ENABLED: bool> Eq for SpinTracingTimestamp<ENABLED> where
    BoolSelector<ENABLED>: TimestampSelector<ENABLED>
{
}

impl SpinTracingTimestamp<false> {
    /// Creates a disabled `SpinTracingTimestamp`.
    #[inline(always)]
    pub const fn new() -> Self {
        Self { data: () }
    }

    /// Creates a disabled `SpinTracingTimestamp` ignoring the provided timestamp.
    #[inline(always)]
    pub const fn from_ticks(_ticks: InstantBootTicks) -> Self {
        Self { data: () }
    }

    /// Returns a zero timestamp (`InstantBootTicks(0)`).
    #[inline(always)]
    pub const fn value(&self) -> InstantBootTicks {
        InstantBootTicks(0)
    }
}

impl Default for SpinTracingTimestamp<false> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl SpinTracingTimestamp<true> {
    /// Captures the current `KTrace::timestamp()`.
    #[inline]
    pub fn new() -> Self {
        Self { data: KTrace::timestamp() }
    }

    /// Creates a `SpinTracingTimestamp` from an explicit timestamp.
    #[inline(always)]
    pub const fn from_ticks(ticks: InstantBootTicks) -> Self {
        Self { data: ticks }
    }

    /// Returns the captured timestamp.
    #[inline(always)]
    pub const fn value(&self) -> InstantBootTicks {
        self.data
    }
}

impl Default for SpinTracingTimestamp<true> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

zr::static_assert!(size_of::<SpinTracingTimestamp<false>>() == 0);
zr::static_assert!(size_of::<SpinTracingTimestamp<true>>() == 8);

/// Records the duration spent spinning on a lock and emits a `"lock_spin"` ktrace record on
/// completion when `TRACE_INSTRUMENTED` is true.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Tracer<const TRACE_INSTRUMENTED: bool = false>
where
    BoolSelector<TRACE_INSTRUMENTED>: TimestampSelector<TRACE_INSTRUMENTED>,
{
    data: <BoolSelector<TRACE_INSTRUMENTED> as TimestampSelector<TRACE_INSTRUMENTED>>::Data,
}

impl Tracer<false> {
    /// Creates a no-op `Tracer`.
    #[inline(always)]
    pub const fn new() -> Self {
        Self { data: () }
    }

    /// Creates a no-op `Tracer` ignoring the starting timestamp.
    #[inline(always)]
    pub const fn from_ticks(_ticks: InstantBootTicks) -> Self {
        Self { data: () }
    }

    /// Creates a no-op `Tracer` ignoring the raw starting ticks.
    #[inline(always)]
    pub const fn from_raw_ticks(_ticks: i64) -> Self {
        Self { data: () }
    }

    /// No-op when tracing is disabled.
    #[inline(always)]
    pub const fn finish(&self, _finish_type: FinishType, _elid: EncodedLockId) {}

    /// No-op when tracing is disabled.
    #[inline(always)]
    pub const fn finish_with_timestamp(
        &self,
        _finish_type: FinishType,
        _elid: EncodedLockId,
        _end_time: SpinTracingTimestamp<false>,
    ) {
    }
}

impl Default for Tracer<false> {
    #[inline(always)]
    fn default() -> Self {
        Self::new()
    }
}

impl Tracer<true> {
    /// Creates an instrumented `Tracer` capturing `KTrace::timestamp()` as the start time.
    #[inline]
    pub fn new() -> Self {
        Self { data: KTrace::timestamp() }
    }

    /// Creates an instrumented `Tracer` with an explicit starting timestamp.
    #[inline(always)]
    pub const fn from_ticks(ticks: InstantBootTicks) -> Self {
        Self { data: ticks }
    }

    /// Creates an instrumented `Tracer` from raw ticks.
    #[inline(always)]
    pub const fn from_raw_ticks(ticks: i64) -> Self {
        Self { data: InstantBootTicks(ticks) }
    }

    /// Completes the spin trace event using the current `KTrace::timestamp()` as the end time.
    #[inline]
    pub fn finish(&self, finish_type: FinishType, elid: EncodedLockId) {
        let category = resolve_category!("kernel:sched");
        if !KTrace::category_enabled(category) {
            return;
        }
        self.finish_with_timestamp(finish_type, elid, SpinTracingTimestamp::<true>::new());
    }

    /// Completes the spin trace event using an explicit `end_time`.
    #[inline]
    pub fn finish_with_timestamp(
        &self,
        finish_type: FinishType,
        elid: EncodedLockId,
        end_time: SpinTracingTimestamp<true>,
    ) {
        let category = resolve_category!("kernel:sched");
        if !KTrace::category_enabled(category) {
            return;
        }

        let class_name = if elid.class_name() != IdStringRef::INVALID_ID {
            elid.class_name()
        } else {
            resolve_string!("<unknown>").id()
        };

        let name = resolve_string!("lock_spin");
        let lock_class_key = resolve_string!("lock_class");

        if !SCHEDULER_LOCK_SPIN_TRACING_COMPRESSED {
            let lock_id = elid.id();
            let lock_type = if elid.lock_type() == LockType::Spinlock {
                resolve_string!("Spinlock")
            } else {
                resolve_string!("Mutex")
            };
            let blocked_after = finish_type == FinishType::Blocked;

            KTrace::emit_event(
                EventType::DurationComplete,
                category,
                name,
                self.data,
                Context::Thread,
                Some(end_time.value().0 as u64),
                &[
                    Argument::new(resolve_string!("lock_id"), lock_id),
                    Argument::new(lock_class_key, IdStringRef::new(class_name)),
                    Argument::new(resolve_string!("lock_type"), lock_type),
                    Argument::new(resolve_string!("blocked_after"), blocked_after),
                ],
                &[],
            );
        } else {
            // TODO(johngro): We could compress this even more if we omitted the
            // dedicated lock_class field (we can extract the value from the elid
            // instead). To do this, I need to update my external scripts to process
            // the native FXT format. Currently, it depends on conversion to JSON
            // before processing, which strips the string table from the data
            // (embedding it directly into the JSON instead).
            KTrace::emit_event(
                EventType::DurationComplete,
                category,
                name,
                self.data,
                Context::Thread,
                Some(end_time.value().0 as u64),
                &[
                    Argument::new(lock_class_key, IdStringRef::new(class_name)),
                    Argument::new(resolve_string!("elid"), elid.finished_value(finish_type)),
                ],
                &[],
            );
        }
    }
}

impl Default for Tracer<true> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

zr::static_assert!(size_of::<Tracer<false>>() == 0);
zr::static_assert!(size_of::<Tracer<true>>() == 8);

/// Spin tracing unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "spin_tracing")]
mod tests {
    use core::mem::{align_of_val, size_of_val};
    use core::ptr;
    use unittest::{expect_eq, expect_ne, expect_true};

    /// Verifies bitfield mask, encoding, decoding, and resetting.
    #[test]
    fn test_bitfield_basic() {
        type B = internal::Bitfield<4, 8>;
        expect_eq!(B::MASK, 0xF);
        expect_eq!(B::encode(0x5), 0x500);
        expect_eq!(B::decode(0x500), 0x5);
        expect_eq!(B::decode(0xF5AA), 0x5);
        expect_eq!(B::reset(0x1234, 0xA), 0x1A34);
    }

    /// Verifies round-trip conversions for `LockType` and `FinishType`.
    #[test]
    fn test_lock_and_finish_types() {
        expect_eq!(LockType::from_u8(LOCK_TYPE_SPINLOCK), LockType::Spinlock);
        expect_eq!(LockType::from_u8(LOCK_TYPE_MUTEX), LockType::Mutex);
        expect_eq!(LockType::from_u8(LOCK_TYPE_CHAIN_LOCK), LockType::ChainLock);
        expect_eq!(LockType::from_u8(LOCK_TYPE_RW_LOCK), LockType::RwLock);

        expect_eq!(FinishType::from_u8(FINISH_TYPE_LOCK_ACQUIRED), FinishType::LockAcquired);
        expect_eq!(FinishType::from_u8(FINISH_TYPE_BLOCKED), FinishType::Blocked);
    }

    /// Verifies default/invalid `EncodedLockId` state.
    #[test]
    fn test_encoded_lock_id_invalid() {
        let invalid = EncodedLockId::invalid();
        expect_eq!(invalid.value(), 0);
        expect_eq!(invalid.id(), 0);
        expect_eq!(invalid.class_name(), 0);
        expect_eq!(invalid.lock_type(), LockType::Spinlock);
        expect_eq!(invalid.finish_type(), FinishType::LockAcquired);
    }

    /// Verifies packing, updating, and extracting fields in `EncodedLockId`.
    #[test]
    fn test_encoded_lock_id_fields() {
        let lock_id = 0x1234_5678_9ABC;
        let class_id = 0x5A5;
        let mut elid = EncodedLockId::new(LockType::Mutex, lock_id, class_id);

        expect_eq!(elid.id(), lock_id);
        expect_eq!(elid.class_name(), class_id);
        expect_eq!(elid.lock_type(), LockType::Mutex);
        expect_eq!(elid.finish_type(), FinishType::LockAcquired);

        elid.set_lock_class_id(0x789);
        expect_eq!(elid.class_name(), 0x789);
        expect_eq!(elid.id(), lock_id);
        expect_eq!(elid.lock_type(), LockType::Mutex);

        let finished = elid.finished_value(FinishType::Blocked);
        let finished_elid = EncodedLockId::from_raw(finished);
        expect_eq!(finished_elid.finish_type(), FinishType::Blocked);
        expect_eq!(finished_elid.id(), lock_id);
        expect_eq!(finished_elid.class_name(), 0x789);
        expect_eq!(finished_elid.lock_type(), LockType::Mutex);
    }

    /// Verifies boundary maximum values for all bitfields in `EncodedLockId`.
    #[test]
    fn test_encoded_lock_id_max_values() {
        let max_id = (1u64 << 49) - 1;
        let max_class = (1u16 << 12) - 1;
        let elid = EncodedLockId::new(LockType::RwLock, max_id, max_class);

        expect_eq!(elid.id(), max_id);
        expect_eq!(elid.class_name(), max_class);
        expect_eq!(elid.lock_type(), LockType::RwLock);
        expect_eq!(elid.finish_type(), FinishType::LockAcquired);

        let blocked_raw = elid.finished_value(FinishType::Blocked);
        let blocked = EncodedLockId::from_raw(blocked_raw);
        expect_eq!(blocked.finish_type(), FinishType::Blocked);
        expect_eq!(blocked.id(), max_id);
        expect_eq!(blocked.class_name(), max_class);
        expect_eq!(blocked.lock_type(), LockType::RwLock);
        expect_eq!(blocked.value(), u64::MAX);
    }

    /// Verifies disabled `LockNameStorage` is zero-sized and returns disabled references.
    #[test]
    fn test_storage_disabled_size_and_methods() {
        let mut storage = LockNameStorage::<LOCK_TYPE_SPINLOCK, false>::new();
        expect_eq!(size_of_val(&storage), 0);
        expect_eq!(storage.encoded_lock_id().value(), EncodedLockId::INVALID.value());
        expect_eq!(storage.class_name_ref().id(), IdStringRef::disabled().id());
        expect_ne!(storage.class_name_ref().id(), IdStringRef::INVALID_ID);

        // SAFETY: `set_lock_class_id` accepts either a null pointer or a pointer to a valid
        // `MetadataLockClassState`. `ptr::null()` is null, satisfying the first branch.
        unsafe { storage.set_lock_class_id(ptr::null()) };
        expect_eq!(storage.encoded_lock_id().value(), EncodedLockId::INVALID.value());

        let storage_with_id =
            LockNameStorage::<LOCK_TYPE_SPINLOCK, false>::with_class_name_id(1234);
        expect_eq!(size_of_val(&storage_with_id), 0);
        expect_eq!(storage_with_id.encoded_lock_id().value(), EncodedLockId::INVALID.value());
    }

    /// Verifies enabled `LockNameStorage` layout, ID generation, and class name resolution.
    #[test]
    fn test_storage_enabled_size_and_methods() {
        let mut storage = LockNameStorage::<LOCK_TYPE_SPINLOCK, true>::new();
        expect_eq!(size_of_val(&storage), 8);
        expect_eq!(align_of_val(&storage), 8);

        let elid = storage.encoded_lock_id();
        expect_ne!(elid.id(), 0);
        expect_eq!(elid.lock_type(), LockType::Spinlock);
        expect_eq!(elid.class_name(), IdStringRef::INVALID_ID);
        expect_eq!(storage.class_name_ref().id(), IdStringRef::invalid().id());
        expect_ne!(storage.class_name_ref().id(), IdStringRef::INVALID_ID);

        storage.set_class_name_id(42);
        expect_eq!(storage.encoded_lock_id().class_name(), 42);
        expect_eq!(storage.class_name_ref().id(), 42);

        let storage2 = LockNameStorage::<LOCK_TYPE_MUTEX, true>::with_class_name_id(99);
        expect_eq!(storage2.encoded_lock_id().lock_type(), LockType::Mutex);
        expect_eq!(storage2.encoded_lock_id().class_name(), 99);
        expect_ne!(storage2.encoded_lock_id().id(), elid.id());

        // Verify setting lock class ID from a simulated MetadataLockClassState pointer.
        let mut storage3 = LockNameStorage::<LOCK_TYPE_SPINLOCK, true>::new();
        let test_interned: &'static InternedString = resolve_string!("test_lock_class");
        let fake_metadata_state: *const InternedString = ptr::from_ref(test_interned);
        // SAFETY: `set_lock_class_id` requires a non-null `lcid` to be aligned for
        // `*const InternedString`, valid for reads of `size_of::<*const InternedString>()` bytes
        // during the call, unmutated during the call, and initialized to a pointer to a `'static`
        // `InternedString` in `__fxt_interned_string_table`. `ptr::from_ref(&fake_metadata_state)`
        // points to the live stack local `fake_metadata_state` with no mutable aliases, and its
        // value points to `test_interned`, a `'static` `InternedString` placed in
        // `__fxt_interned_string_table` by `resolve_string!`.
        unsafe {
            storage3.set_lock_class_id(ptr::from_ref(&fake_metadata_state).cast::<c_void>());
        }
        expect_eq!(storage3.encoded_lock_id().class_name(), test_interned.id());
    }

    /// Verifies `const_init` and lazy ID assignment via `ensure_id`.
    #[test]
    fn test_const_init_and_ensure_id() {
        let mut storage = LockNameStorage::<LOCK_TYPE_CHAIN_LOCK, true>::const_init(55);
        expect_eq!(storage.encoded_lock_id().id(), 0);
        expect_eq!(storage.encoded_lock_id().lock_type(), LockType::ChainLock);
        expect_eq!(storage.encoded_lock_id().class_name(), 55);

        storage.ensure_id();
        expect_ne!(storage.encoded_lock_id().id(), 0);
        expect_eq!(storage.encoded_lock_id().lock_type(), LockType::ChainLock);
        expect_eq!(storage.encoded_lock_id().class_name(), 55);
    }

    /// Verifies generic operations across `LockNameStorageOps`.
    #[test]
    fn test_storage_ops_trait() {
        fn test_generic<S: LockNameStorageOps>(mut storage: S) {
            storage.ensure_id();
            let _ = storage.encoded_lock_id();
            let _ = storage.class_name_ref();
            // SAFETY: `set_lock_class_id` accepts either a null pointer or a pointer to a valid
            // `MetadataLockClassState`. `ptr::null()` is null, satisfying the first branch.
            unsafe { storage.set_lock_class_id(ptr::null()) };
        }

        test_generic(LockNameStorage::<LOCK_TYPE_SPINLOCK, false>::new());
        test_generic(LockNameStorage::<LOCK_TYPE_SPINLOCK, true>::new());
    }

    /// Verifies `SpinTracingTimestamp` and `Tracer` behavior in both disabled and enabled modes.
    #[test]
    fn test_timestamp_and_tracer() {
        let ts_disabled = SpinTracingTimestamp::<false>::new();
        expect_eq!(size_of_val(&ts_disabled), 0);
        expect_eq!(ts_disabled.value().0, 0);

        let ts_enabled = SpinTracingTimestamp::<true>::new();
        expect_eq!(size_of_val(&ts_enabled), 8);
        expect_true!(ts_enabled.value().0 > 0);

        let ts_explicit = SpinTracingTimestamp::<true>::from_ticks(InstantBootTicks(12345));
        expect_eq!(ts_explicit.value().0, 12345);

        let tracer_disabled = Tracer::<false>::new();
        expect_eq!(size_of_val(&tracer_disabled), 0);
        tracer_disabled.finish(FinishType::LockAcquired, EncodedLockId::INVALID);
        tracer_disabled.finish_with_timestamp(
            FinishType::Blocked,
            EncodedLockId::INVALID,
            ts_disabled,
        );

        let tracer_enabled = Tracer::<true>::new();
        expect_eq!(size_of_val(&tracer_enabled), 8);
        let test_spinlock_id = resolve_string!("test_spinlock").id();
        let elid = EncodedLockId::new(LockType::Spinlock, 7, test_spinlock_id);
        tracer_enabled.finish(FinishType::LockAcquired, elid);
        tracer_enabled.finish_with_timestamp(
            FinishType::Blocked,
            EncodedLockId::INVALID,
            SpinTracingTimestamp::<true>::new(),
        );
    }
}
