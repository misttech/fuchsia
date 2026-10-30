// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 controller implementation.

use core::ffi::c_void;
use core::fmt::Write as _;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use debug::dprintf;
use fbl::{
    Array, DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    Recyclable, RefPtr, TrackingSize, pin_make_ref_counted,
};
use ksync::{KCell, KMutex, LockToken};
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;

use crate::arm_smmu::common::{
    ArmSmmuMode, DRIVER_OOPS_BANNER, EscapedName, InterruptPolarity, InterruptTriggerMode, SmmuEnv,
    SmmuNoIrqSavePolicy, SmmuRawSpinLock, ZX_MAX_NAME_LEN, ZX_RSRC_KIND_MMIO,
};
use crate::arm_smmu::v2::context_bank::ContextBank;
use crate::arm_smmu::v2::registers::gr0::{
    ClientPd, Cr0, Cr2, ExidEnable, Gfar, Gfsr, Gfsynr0, Gfsynr1, Idr0, Idr1, Idr2, Idr7, S2cr,
    S2crBypass, S2crFault, S2crTranslation, S2crType, Smr, Stalld, Tlbgstatus, Tlbgsync,
    Tlbiallnsnh,
};
use crate::arm_smmu::v2::registers::gr1::{
    Cba2r, Cba2rVa64, Cbar, CbarS1TS2Bypass, CbarS1TS2Fault, CbarS1TS2Translate, CbarS2Translation,
    CbarType, Cbfrsynra,
};
use crate::arm_smmu::v2::registers::s1cbr::{
    Far, Fsr, Fsynr0, Sctlr, Tcr, Tcr32Bit, Tcr64Bit, TcrExt32Bit, Ttbr0_32Bit, Ttbr0_64Bit,
    Ttbr0Ext32Bit, Ttbr1_32Bit, Ttbr1_64Bit, Ttbr1Ext32Bit,
};
use crate::arm_smmu::v2::registers::{FieldDesc, SmmuMmio};
use crate::arm_smmu::v2::smmu_bti::SmmuBti;
use crate::arm_smmu::v2::stream_match_reg::StreamMatchRegGroup;
use crate::arm_smmu::v2::utils::{
    AddrMode, Bitmask128, BtiMode, SmmuParams, SmrValue, arm_cbar_type_to_string,
    arm_s2cr_type_to_string, arm_smmu_mode_to_string, bti_mode_to_string,
};

/// ZBI ARM SMMU IRQ flag indicating rising-edge trigger (`ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE: u32 = 1 << 0;

/// ZBI ARM SMMU IRQ flag indicating falling-edge trigger (`ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE: u32 = 1 << 1;

/// ZBI ARM SMMU IRQ flag indicating active-high level polarity
/// (`ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH: u32 = 1 << 2;

/// ZBI ARM SMMU IRQ flag indicating active-low level polarity
/// (`ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW`).
pub const ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW: u32 = 1 << 3;

/// Maximum number of IRQ descriptors in [`ZbiDcfgArmSmmuDriver::irqs`].
pub const ZBI_ARM_SMMU_MAX_IRQS: usize = 128;

/// Maximum number of handoff SMR entries in [`ZbiDcfgArmSmmuDriver::handoff_smrs`].
pub const ZBI_ARM_SMMU_MAX_HANDOFF_SMRS: usize = 16;

/// ZBI IRQ descriptor for an ARM SMMUv2 interrupt line.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZbiDcfgArmSmmuIrq {
    /// Interrupt number (GIC SPI/PPI).
    pub num: u32,
    /// Interrupt configuration flags (`ZBI_ARM_SMMU_IRQ_FLAGS_*`).
    pub flags: u32,
}

zr::static_assert!(core::mem::size_of::<ZbiDcfgArmSmmuIrq>() == 8);
zr::static_assert!(core::mem::align_of::<ZbiDcfgArmSmmuIrq>() == 4);

/// ZBI driver configuration payload for an ARM SMMUv2 instance.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ZbiDcfgArmSmmuDriver {
    /// Base physical address of the SMMU address space (Global Register Space 0).
    pub mmio_phys: u64,
    /// Number of context banks that a driver should limit itself to using (`0` for no override).
    pub num_context_banks_override: u32,
    /// Number of stream match registers that a driver should limit itself to using (`0` for no
    /// override).
    pub num_smr_override: u32,
    /// Total number of valid entries in `irqs`.
    pub irq_cnt: u32,
    /// Number of global fault interrupt entries at the start of `irqs`.
    pub global_irq_cnt: u32,
    /// Interrupt descriptors (`[0, global_irq_cnt)` global, `[global_irq_cnt, irq_cnt)` context).
    pub irqs: [ZbiDcfgArmSmmuIrq; ZBI_ARM_SMMU_MAX_IRQS],
    /// Number of valid entries in `handoff_smrs`.
    pub handoff_smr_cnt: u32,
    /// Stream ID ranges (`(MASK << 16) | ID`) handed off from the bootloader.
    pub handoff_smrs: [u32; ZBI_ARM_SMMU_MAX_HANDOFF_SMRS],
}

zr::static_assert!(core::mem::size_of::<ZbiDcfgArmSmmuDriver>() == 1120);
zr::static_assert!(core::mem::align_of::<ZbiDcfgArmSmmuDriver>() == 8);

impl Default for ZbiDcfgArmSmmuDriver {
    fn default() -> Self {
        Self {
            mmio_phys: 0,
            num_context_banks_override: 0,
            num_smr_override: 0,
            irq_cnt: 0,
            global_irq_cnt: 0,
            irqs: [ZbiDcfgArmSmmuIrq::default(); ZBI_ARM_SMMU_MAX_IRQS],
            handoff_smr_cnt: 0,
            handoff_smrs: [0; ZBI_ARM_SMMU_MAX_HANDOFF_SMRS],
        }
    }
}

/// IOMMU descriptor for fetching an ARM SMMUv2 instance via `zx_iommu_create`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZxIommuDescArmSmmu {
    /// Physical base address of the SMMU instance to bind.
    pub register_base: u64,
}

zr::static_assert!(core::mem::size_of::<ZxIommuDescArmSmmu>() == 8);
zr::static_assert!(core::mem::align_of::<ZxIommuDescArmSmmu>() == 8);

ksync::declare_singleton_lock!(pub(crate) SmmuInstanceLock);

pub(crate) struct GlobalInstanceList {
    initialized: core::cell::UnsafeCell<bool>,
    inner: core::cell::UnsafeCell<
        core::mem::MaybeUninit<DoublyLinkedList<RefPtr<Smmu>, DefaultObjectTag, TrackingSize>>,
    >,
}

// SAFETY: Access to `GlobalInstanceList` is synchronized via `SmmuInstanceLock`.
unsafe impl Sync for GlobalInstanceList {}

impl GlobalInstanceList {
    const fn new() -> Self {
        Self {
            initialized: core::cell::UnsafeCell::new(false),
            inner: core::cell::UnsafeCell::new(core::mem::MaybeUninit::uninit()),
        }
    }

    pub(crate) fn get_mut<'a>(
        &'a self,
        _token: &'a mut LockToken<'_, SmmuInstanceLock>,
    ) -> Pin<&'a mut DoublyLinkedList<RefPtr<Smmu>, DefaultObjectTag, TrackingSize>> {
        // SAFETY: Synchronized by the mutable `LockToken` for `SmmuInstanceLock`, which stays
        // borrowed for as long as the returned reference is live, so that two calls cannot hand
        // out two live `&mut` references to the list.
        unsafe {
            let init = &mut *self.initialized.get();
            let slot = &mut *self.inner.get();
            if !*init {
                let Ok(()) =
                    pin_init::PinInit::__pinned_init(DoublyLinkedList::new(), slot.as_mut_ptr());
                *init = true;
            }
            Pin::new_unchecked(slot.assume_init_mut())
        }
    }
}

pub(crate) static INSTANCES: GlobalInstanceList = GlobalInstanceList::new();

// Lock protecting `FAULT_BUFFER`. This is a spinlock (acquired at hard IRQ time, with a BTI's
// spinlock potentially held) in the kernel, and a plain mutex in host tests.
#[cfg(is_kernel)]
ksync::declare_singleton_lock!(FaultBufferLock, ::ksync::RawSpinlock);
#[cfg(not(is_kernel))]
ksync::declare_singleton_lock!(FaultBufferLock);

/// Size in bytes of the global buffer used to render context fault reports.
const FAULT_BUFFER_LEN: usize = 1024;

/// Global buffer used to render a context fault report before printing it in one piece.
struct FaultBuffer {
    buf: core::cell::UnsafeCell<[u8; FAULT_BUFFER_LEN]>,
}

// SAFETY: Access to `FaultBuffer` is synchronized via `FaultBufferLock`.
unsafe impl Sync for FaultBuffer {}

impl FaultBuffer {
    const fn new() -> Self {
        Self { buf: core::cell::UnsafeCell::new([0u8; FAULT_BUFFER_LEN]) }
    }

    fn get_mut<'a>(
        &'a self,
        _token: &'a mut LockToken<'_, FaultBufferLock>,
    ) -> &'a mut [u8; FAULT_BUFFER_LEN] {
        // SAFETY: Synchronized by the mutable `LockToken` for `FaultBufferLock`, which is borrowed
        // for as long as the returned reference is live.
        unsafe { &mut *self.buf.get() }
    }
}

static FAULT_BUFFER: FaultBuffer = FaultBuffer::new();

/// `core::fmt::Write` adapter which renders into a fixed byte buffer, silently truncating (on a
/// character boundary) once the buffer is full.
struct FaultReportWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> FaultReportWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    fn as_str(&self) -> &str {
        // SAFETY: `write_str` only ever appends whole UTF-8 characters to `self.buf[..self.len]`.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl core::fmt::Write for FaultReportWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let avail = self.buf.len() - self.len;
        let mut to_copy = s.len().min(avail);
        // Never split a multi-byte character when truncating.
        while !s.is_char_boundary(to_copy) {
            to_copy -= 1;
        }
        self.buf[self.len..self.len + to_copy].copy_from_slice(&s.as_bytes()[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

/// Parsed IRQ definition for a global or context bank interrupt line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqDef {
    /// Interrupt vector number.
    pub num: u32,
    /// Interrupt flags (`ZBI_ARM_SMMU_IRQ_FLAGS_*`).
    pub flags: u32,
}

impl IrqDef {
    /// Constructs an [`IrqDef`] from a [`ZbiDcfgArmSmmuIrq`].
    #[inline]
    pub const fn from_zbi(irq: ZbiDcfgArmSmmuIrq) -> Self {
        Self { num: irq.num, flags: irq.flags }
    }

    /// Returns `true` if exactly one valid trigger/polarity bit is set in `self.flags`.
    #[inline]
    pub const fn valid(&self) -> bool {
        const VALID_BITS: u32 = ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE
            | ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE
            | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH
            | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW;
        (self.flags & VALID_BITS).count_ones() == 1
    }

    /// Returns the [`InterruptTriggerMode`] for this IRQ.
    #[inline]
    pub const fn trigger_mode(&self) -> InterruptTriggerMode {
        if (self.flags & (ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE | ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE))
            != 0
        {
            InterruptTriggerMode::Edge
        } else {
            InterruptTriggerMode::Level
        }
    }

    /// Returns the [`InterruptPolarity`] for this IRQ.
    #[inline]
    pub const fn polarity(&self) -> InterruptPolarity {
        if (self.flags & (ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE | ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH))
            != 0
        {
            InterruptPolarity::High
        } else {
            InterruptPolarity::Low
        }
    }
}

/// The cookie registered with the interrupt controller for one global or context interrupt
/// vector: which SMMU the vector belongs to, and its index in that SMMU's vector table.
///
/// Temporary porting note: in C++ these two members live in the vector structures themselves
/// (`GlobalIrqVector` / `ContextIrqVector`), and the cookie is a pointer to the vector. The
/// interrupt thunks read them without holding `irq_lock`, which is fine in C++ because they never
/// change after initialization. In Rust the vector tables are guarded by `irq_lock` and are
/// accessed through `&mut` references to the whole table, which an unlocked read of one of their
/// elements would alias. So the two immutable members are kept here, outside of the guarded
/// tables.
#[derive(Default)]
pub(crate) struct IrqCookie {
    smmu: *const Smmu,
    ndx: u32,
}

/// Global fault interrupt vector state.
#[derive(Default)]
pub(crate) struct GlobalIrqVector {
    pub(crate) irq_def: IrqDef,
    pub(crate) registered: bool,
}

/// Context bank fault interrupt vector state.
#[derive(Default)]
pub(crate) struct ContextIrqVector {
    pub(crate) irq_def: IrqDef,
    pub(crate) enabled: bool,
    pub(crate) registered: bool,
    pub(crate) in_flight: bool,
    pub(crate) bti: Option<RefPtr<SmmuBti>>,
}

/// Init-time immutable hardware configuration cached during `Smmu::init`.
#[derive(Debug, Default)]
pub(crate) struct SmmuInitState {
    /// Stashed debug name formatted during initialization.
    pub(crate) name_buf: [u8; 32],
    pub(crate) name_len: usize,
    /// Operating mode determined during init from boot options and hardware capabilities.
    pub(crate) op_mode: ArmSmmuMode,
    /// Cached copies of read-only ID registers fetched during initialization.
    pub(crate) idr0: Idr0,
    pub(crate) idr1: Idr1,
    pub(crate) idr2: Idr2,
    pub(crate) idr7: Idr7,
    /// Mask of valid Stream ID bits (`((1 << NUMSIDB) - 1) | (((1 << NUMSIDB) - 1) << 16)`).
    pub(crate) valid_stream_id_mask: u32,
    /// Number of context banks and stream match register groups actually used by this driver.
    /// `num_cbs` uses an atomic so it can be reduced to `context_irq_cnt` after IRQ registration
    /// and `lockdown` without creating an exclusive `&mut SmmuInitState` reference while IRQ
    /// handlers are registered.
    pub(crate) num_cbs: AtomicU32,
    pub(crate) num_smrgs: u32,
    /// Virtual address bases of the Global 0, Global 1, and Context Bank 0 register spaces.
    pub(crate) gr0_base: SmmuMmio,
    pub(crate) gr1_base: SmmuMmio,
    pub(crate) cb0_base: SmmuMmio,
}

/// An implementation of an `Iommu` based on the ARM SMMUv2 specification:
/// "ARM System Memory Management Unit Architecture Specification - SMMU architecture version 2.0"
/// Document ID: ARM IHI 0062D.c (ID070417).
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct Smmu {
    #[pin]
    pub(crate) list_node: DoublyLinkedListNode<Smmu>,
    phys_reg_base: u64,
    init_state: core::cell::UnsafeCell<SmmuInitState>,
    /// The cookies registered with the interrupt controller, one per entry of `global_irqs` and
    /// `context_irqs`. Like `init_state`, these are written once in `init()`, before any handler
    /// is registered, and are immutable from then on. See `IrqCookie`.
    global_irq_cookies: core::cell::UnsafeCell<Array<IrqCookie>>,
    context_irq_cookies: core::cell::UnsafeCell<Array<IrqCookie>>,

    #[mutex]
    pub(crate) lock: KMutex,
    #[guarded_by(lock)]
    pub(crate) user_mode_bound: bool,
    #[pin]
    #[guarded_by(lock)]
    pub(crate) bti_list: DoublyLinkedList<RefPtr<SmmuBti>, DefaultObjectTag, TrackingSize>,
    /// Each SMMU may have up to 128 either stream match register groups, or
    /// context banks, for us to allocate and associate with BTIs. Keep a 128-bit
    /// bitmask for each resource type which tracks which registers/context banks
    /// are still available for allocation (1 means available, 0 means currently in
    /// use).
    #[guarded_by(lock)]
    pub(crate) available_smrgs: Bitmask128,
    #[guarded_by(lock)]
    pub(crate) available_cbs: Bitmask128,

    #[mutex]
    pub(crate) irq_lock: KMutex<SmmuRawSpinLock>,
    #[guarded_by(irq_lock)]
    pub(crate) global_irqs: Array<GlobalIrqVector>,
    #[guarded_by(irq_lock)]
    pub(crate) context_irqs: Array<ContextIrqVector>,
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for Smmu {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: Mutable fields of `Smmu` are either initialized once in `init()` before publication or
// synchronized by `lock`, `irq_lock`, or `SmmuInstanceLock`.
unsafe impl Send for Smmu {}
// SAFETY: Shared access to `Smmu` synchronizes mutable state via `lock`, `irq_lock`, or
// `SmmuInstanceLock`.
unsafe impl Sync for Smmu {}

#[pinned_drop]
impl PinnedDrop for Smmu {
    fn drop(mut self: Pin<&mut Self>) {
        // By the time that we destruct, all of our interrupts should be unregistered.
        // Assert this.
        {
            let this = self.as_ref().get_ref();
            ksync::lock!(let irq_guard = this.lock_irq_lock());
            for vec in irq_guard.global_irqs().iter() {
                debug_assert!(!vec.registered);
            }
            for vec in irq_guard.context_irqs().iter() {
                debug_assert!(!vec.registered);
            }
        }
        {
            let this = self.as_ref().get_ref();
            ksync::lock!(let guard = this.lock_lock());
            debug_assert!(guard.bti_list().is_empty());
        }
        // SAFETY: `bti_list` was verified empty above, and we are in `PinnedDrop::drop`.
        unsafe {
            let mut_ref = self.as_mut().get_unchecked_mut();
            Pin::new_unchecked(mut_ref.bti_list.get_inner_mut()).clear();
        }
    }
}

extern "C" fn global_irq_thunk(cookie: *mut c_void) {
    // SAFETY: `cookie` was registered in `register_global_irqs` pointing to an `IrqCookie` in
    // `Smmu::global_irq_cookies`, which is never written after `init()` and which lives, along
    // with the `Smmu` it points to, until after `unregister_interrupts` completes.
    unsafe {
        let cookie = &*(cookie as *const IrqCookie);
        (*cookie.smmu).handle_global_irq(cookie.ndx);
    }
}

extern "C" fn context_irq_thunk(cookie: *mut c_void) {
    // SAFETY: `cookie` was registered in `register_context_irqs` pointing to an `IrqCookie` in
    // `Smmu::context_irq_cookies`, which is never written after `init()` and which lives, along
    // with the `Smmu` it points to, until after `unregister_interrupts` completes.
    unsafe {
        let cookie = &*(cookie as *const IrqCookie);
        (*cookie.smmu).handle_context_irq(cookie.ndx);
    }
}

trait IrqVectorEntry: Sized {
    const KIND: &'static str;
    const THUNK: extern "C" fn(*mut c_void);

    fn irq_def(&self) -> IrqDef;
    fn registered(&self) -> bool;
    fn set_registered(&mut self, registered: bool);
    fn cookies(smmu: &Smmu) -> &core::cell::UnsafeCell<Array<IrqCookie>>;
    fn slice<'b>(guard: &'b SmmuIrqLockGuard<'_>) -> &'b [Self];
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuIrqLockGuard<'_>>) -> &'b mut [Self];
}

impl IrqVectorEntry for GlobalIrqVector {
    const KIND: &'static str = "Global";
    const THUNK: extern "C" fn(*mut c_void) = global_irq_thunk;

    #[inline]
    fn irq_def(&self) -> IrqDef {
        self.irq_def
    }

    #[inline]
    fn registered(&self) -> bool {
        self.registered
    }

    #[inline]
    fn set_registered(&mut self, registered: bool) {
        self.registered = registered;
    }

    #[inline]
    fn cookies(smmu: &Smmu) -> &core::cell::UnsafeCell<Array<IrqCookie>> {
        &smmu.global_irq_cookies
    }

    #[inline]
    fn slice<'b>(guard: &'b SmmuIrqLockGuard<'_>) -> &'b [Self] {
        guard.global_irqs()
    }

    #[inline]
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuIrqLockGuard<'_>>) -> &'b mut [Self] {
        guard.global_irqs_mut()
    }
}

impl IrqVectorEntry for ContextIrqVector {
    const KIND: &'static str = "Context";
    const THUNK: extern "C" fn(*mut c_void) = context_irq_thunk;

    #[inline]
    fn irq_def(&self) -> IrqDef {
        self.irq_def
    }

    #[inline]
    fn registered(&self) -> bool {
        self.registered
    }

    #[inline]
    fn set_registered(&mut self, registered: bool) {
        self.registered = registered;
    }

    #[inline]
    fn cookies(smmu: &Smmu) -> &core::cell::UnsafeCell<Array<IrqCookie>> {
        &smmu.context_irq_cookies
    }

    #[inline]
    fn slice<'b>(guard: &'b SmmuIrqLockGuard<'_>) -> &'b [Self] {
        guard.context_irqs()
    }

    #[inline]
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuIrqLockGuard<'_>>) -> &'b mut [Self] {
        guard.context_irqs_mut()
    }
}

impl Smmu {
    fn format_smmu_name(phys_reg_base: u64) -> ([u8; 32], usize) {
        let mut buf = [0u8; 32];
        let prefix = b"SMMU@0x";
        buf[..prefix.len()].copy_from_slice(prefix);
        let mut pos = prefix.len();
        const HEX: &[u8; 16] = b"0123456789abcdef";

        // Format "SMMU@0x<phys_reg_base>" with at least 8 lowercase hex digits, or
        // more if `phys_reg_base > 0xffff_ffff`.
        let top_nibble = if phys_reg_base > 0xffff_ffff {
            (64 - phys_reg_base.leading_zeros()).div_ceil(4) as usize
        } else {
            8usize
        };
        for i in (0..top_nibble).rev() {
            let nib = ((phys_reg_base >> (i * 4)) & 0xf) as usize;
            buf[pos] = HEX[nib];
            pos += 1;
        }
        (buf, pos)
    }

    #[inline]
    pub(crate) fn init_state(&self) -> &SmmuInitState {
        // SAFETY: Non-atomic fields of `init_state` are only mutated inside `init()` while holding
        // `self.lock` before any interrupt handlers are registered or `self` is published to
        // `INSTANCES`. Post-registration updates to `num_cbs` use atomic operations.
        unsafe { &*self.init_state.get() }
    }

    /// Returns the human-readable instance name (e.g. `"SMMU@0x09000000"`).
    #[inline]
    pub fn name(&self) -> &str {
        let st = self.init_state();
        // SAFETY: `name_buf[..name_len]` is initialized with ASCII characters in
        // `format_smmu_name`.
        unsafe { core::str::from_utf8_unchecked(&st.name_buf[..st.name_len]) }
    }

    /// Returns the global operating mode of this SMMU instance.
    #[inline]
    pub fn op_mode(&self) -> ArmSmmuMode {
        self.init_state().op_mode
    }

    /// Returns the valid Stream ID mask (`(mask << 16) | id`) for this SMMU instance.
    #[inline]
    pub fn valid_stream_id_mask(&self) -> u32 {
        self.init_state().valid_stream_id_mask
    }

    /// Returns the size in bytes of each register page (`4096` or `65536`).
    #[inline]
    pub fn reg_page_size(&self) -> usize {
        if self.init_state().idr1.pagesize() != 0 { 1usize << 16 } else { 1usize << 12 }
    }

    /// Returns the number of register pages (`1 << (IDR1.NUMPAGENDXB + 1)`).
    ///
    /// See Section 9.6.1 : SMMU_IDR0 and SMMU_IDR1 for more details, in addition to Section 8.1.1.
    ///
    /// Please note that the `IDR1.NUMPAGENDXB` comment defining `reg_page_cnt` appears to be
    /// mis-formatted. It says that the number of pages is `2(NUMPAGENDXB + 1)`, but fails to place
    /// the parenthetical in a superscript, or use some other common notation used to indicate
    /// exponentiation as opposed to multiplication. In the absence of a superscript font, either
    /// `2^(NUMPAGENDXB + 1)` or `2**(NUMPAGENDXB + 1)` could be used as an acceptable substitute.
    /// Section 8.1.1 uses a proper superscript notation, and is more correct/clear.
    #[inline]
    pub fn reg_page_cnt(&self) -> usize {
        1usize << (self.init_state().idr1.numpagendxb() + 1)
    }

    /// Returns the effective number of Stream Match Register Groups managed by this SMMU.
    #[inline]
    pub fn num_smrgs(&self) -> u32 {
        self.init_state().num_smrgs
    }

    /// Returns the effective number of Context Banks managed by this SMMU.
    #[inline]
    pub fn num_cbs(&self) -> u32 {
        // `Relaxed` suffices: `num_cbs` is only written during `init()` before `self` is published
        // under `SmmuInstanceLock` (and before any context IRQ is unmasked), so lock release/acquire
        // already orders all post-init reads; `AtomicU32` only avoids `&mut SmmuInitState` after
        // IRQ handlers are registered.
        self.init_state().num_cbs.load(Ordering::Relaxed)
    }

    /// Returns the `GR0` MMIO bank handle.
    #[inline]
    pub fn gr0_base(&self) -> SmmuMmio {
        self.init_state().gr0_base
    }

    /// Returns the `GR1` MMIO bank handle.
    #[inline]
    pub fn gr1_base(&self) -> SmmuMmio {
        self.init_state().gr1_base
    }

    /// Returns the `CB0` MMIO bank handle.
    #[inline]
    pub fn cb0_base(&self) -> SmmuMmio {
        self.init_state().cb0_base
    }

    /// Returns the MMIO bank handle for Context Bank `ndx`.
    #[inline]
    pub fn get_cb_base(&self, ndx: u32) -> SmmuMmio {
        debug_assert!(ndx < self.num_cbs());
        let page_sz = self.reg_page_size();
        self.cb0_base().sub_bank(page_sz * (ndx as usize), page_sz)
    }

    /// Builds an [`SmmuParams`] view implementing `SmmuController` while holding `guard`.
    #[inline]
    pub(crate) fn controller_params<'a>(&'a self, guard: &'a SmmuLockGuard<'_>) -> SmmuParams<'a> {
        SmmuParams {
            name: self.name(),
            op_mode: self.op_mode(),
            num_smrgs: self.num_smrgs(),
            num_cbs: self.num_cbs(),
            available_smrgs: guard.available_smrgs(),
            available_cbs: guard.available_cbs(),
            gr0_base: self.gr0_base(),
            gr1_base: self.gr1_base(),
            cb0_base: self.cb0_base(),
            reg_page_size: self.reg_page_size(),
        }
    }

    /// Returns the [`IrqDef`] for context bank `ndx`, if within bounds.
    pub fn get_context_irq(&self, ndx: u32) -> Option<IrqDef> {
        ksync::lock!(let guard = self.lock_irq_lock());
        let idx = ndx as usize;
        if idx < guard.context_irqs().len() {
            Some(guard.context_irqs()[idx].irq_def)
        } else {
            None
        }
    }

    fn find_instance(
        token: &mut LockToken<'_, SmmuInstanceLock>,
        phys_reg_base: u64,
    ) -> Option<RefPtr<Self>> {
        for smmu in INSTANCES.get_mut(token).iter() {
            if smmu.phys_reg_base == phys_reg_base {
                return Some(RefPtr::from_ref(smmu));
            }
        }
        None
    }

    /// Creates, initializes, and registers a new `Smmu` instance from a ZBI driver config.
    pub fn create(config: &ZbiDcfgArmSmmuDriver) -> Result<RefPtr<Self>, Status> {
        // It is an error to attempt to create an SMMU instance with registers which
        // have already been used.
        ksync::lock!(let mut instance_guard = SmmuInstanceLock::lock());
        if Self::find_instance(instance_guard.as_mut().token_mut(), config.mmio_phys).is_some() {
            return Err(Status::ALREADY_BOUND);
        }

        // Looks like this is a new instance. Allocate an instance, then attempt to
        // initialize it. If we succeed, add the instance to the global list.
        let instance = pin_make_ref_counted!(Smmu {
            list_node: DoublyLinkedListNode::new(),
            phys_reg_base: config.mmio_phys,
            init_state: core::cell::UnsafeCell::new(SmmuInitState::default()),
            global_irq_cookies: core::cell::UnsafeCell::new(Array::new()),
            context_irq_cookies: core::cell::UnsafeCell::new(Array::new()),
            lock <- KMutex::init(),
            user_mode_bound: KCell::new(false),
            bti_list <- KCell::pin_init(DoublyLinkedList::new()),
            available_smrgs: KCell::new(Bitmask128::new()),
            available_cbs: KCell::new(Bitmask128::new()),
            irq_lock <- KMutex::init(),
            global_irqs: KCell::new(Array::new()),
            context_irqs: KCell::new(Array::new()),
        })
        .map_err(|_| Status::NO_MEMORY)?;

        instance.init(config)?;
        INSTANCES.get_mut(instance_guard.as_mut().token_mut()).push_back(instance.clone());
        Ok(instance)
    }

    /// Looks up and binds an `Smmu` instance matching `desc` (a [`ZxIommuDescArmSmmu`]).
    ///
    /// Returns `Ok(Some(smmu))` when the SMMU is in `Passthru` or `Enforced` mode, or `Ok(None)`
    /// when the global or per-instance SMMU mode is `Disabled` (indicating the caller should
    /// substitute a `StubIommu`).
    pub fn fetch(desc: &[u8]) -> Result<Option<RefPtr<Self>>, Status> {
        // If the global SMMU mode is set to Disabled, attempt to create a stub IOMMU
        // for the user. Any SMMUs we have in our global list are only there so we
        // can dump registers using `k` commands.
        if SmmuEnv::boot_option_smmu_mode(None) == ArmSmmuMode::Disabled {
            return Ok(None);
        }

        // Check our parameters, then go looking for an instance which was created
        // during kernel startup which has not already been bound by user-mode.
        let Ok(bytes): Result<[u8; 8], _> = desc.try_into() else {
            dprintf!(INFO, "Bad desc when fetching SMMUv2 ({:p}, {})\n", desc.as_ptr(), desc.len());
            return Err(Status::INVALID_ARGS);
        };

        // TODO(johngro): Currently the user-mode driver framework is creating two
        // IOMMUs for systems with an SMMU. One of them is a StubIommu instance with
        // an ID equal to the SMMU HW's base address. The other is a real SMMU
        // instance, but it gets created using an address with the LSB set in it.
        //
        // This is not correct. They should simply be attempting to create a single
        // SMMU using the base address specified in the device tree. If the SMMU is
        // disabled in the kernel, we will automatically create a Stub for them.
        // Otherwise, we will give them a handle to the real SMMU hardware, provided
        // that no other handle is currently bound to it.
        let register_base = u64::from_ne_bytes(bytes) & !7u64;

        ksync::lock!(let mut instance_guard = SmmuInstanceLock::lock());
        let Some(instance) =
            Self::find_instance(instance_guard.as_mut().token_mut(), register_base)
        else {
            return Err(Status::NOT_FOUND);
        };

        // At this point, we know that the default `ArmSmmuMode` is _not_ disabled.
        // But, if we found the instance requested by the user whose mode has been
        // overridden to disabled, do not return a reference to the disabled
        // instance. Give them a new stub IOMMU instance instead.
        if instance.op_mode() == ArmSmmuMode::Disabled {
            return Ok(None);
        }

        ksync::lock!(let mut guard = instance.lock_lock());
        if !*guard.user_mode_bound() {
            *guard.as_mut().user_mode_bound_mut() = true;
            Ok(Some(instance.clone()))
        } else {
            Err(Status::ALREADY_BOUND)
        }
    }

    /// Cleanly tears down an `Smmu` instance between tests and removes it from the static
    /// `INSTANCES` list while verifying destructor invariants.
    ///
    /// In production kernel boots, `Smmu` instances registered in `INSTANCES` live for the
    /// lifetime of the kernel, whereas unit and model tests create and destroy multiple `Smmu`
    /// instances within a single process.
    #[cfg(any(ktest, not(is_kernel)))]
    pub fn destroy_for_test(smmu: RefPtr<Self>) {
        let mut target: Option<RefPtr<Self>> = None;
        {
            ksync::lock!(let mut instance_guard = SmmuInstanceLock::lock());
            let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
            let found = list.iter().any(|s| core::ptr::eq(s, &*smmu));
            if found {
                // SAFETY: `smmu` was verified to be in `INSTANCES`.
                target = unsafe { list.erase(&*smmu) };
            }
        }
        drop(smmu);
        let Some(target) = target else {
            return;
        };

        // Shut down any remaining adopted or quarantined BTIs before unregistering
        // interrupts so all `ContextBank`, `StreamMatchRegGroup`, and `DeviceAspace`
        // resources are cleanly released.
        loop {
            let bti = {
                ksync::lock!(let guard = target.lock_lock());
                guard.bti_list().front().map(RefPtr::from_ref)
            };
            let Some(bti) = bti else {
                break;
            };

            let mut handled_adopted_without_cb = false;
            {
                ksync::lock!(let mut pmt_guard = bti.lock_pmt_lock());
                ksync::lock!(let mut bti_guard = bti.lock_lock());
                *pmt_guard.as_mut().quarantined_pmt_count_mut() = 0;
                *pmt_guard.as_mut().quarantined_page_count_mut() = 0;
                if *bti_guard.mode() == BtiMode::Adopted {
                    if let Some(ref mut cb) = *bti_guard.as_mut().context_bank_mut() {
                        cb.force_mode_for_test(BtiMode::Fault);
                        *bti_guard.as_mut().mode_mut() = BtiMode::Fault;
                    } else {
                        // Adopted BTI in bypass/fault mode (no ContextBank): disable its SMRGs
                        // directly.
                        while let Some(mut smrg) = bti_guard.as_mut().smrg_list_mut().pop_front() {
                            smrg.disable();
                        }
                        *bti_guard.as_mut().mode_mut() = BtiMode::Shutdown;
                        handled_adopted_without_cb = true;
                    }
                }
            }

            if handled_adopted_without_cb {
                let removed = {
                    ksync::lock!(let mut guard = target.lock_lock());
                    // SAFETY: `bti` is at the front of `target.bti_list`.
                    unsafe { guard.as_mut().bti_list_mut().erase(&*bti) }
                };
                {
                    ksync::lock!(let mut bti_guard = bti.lock_lock());
                    *bti_guard.as_mut().smmu_mut() = None;
                }
                drop(removed);
            } else {
                bti.on_end_of_life();
            }
        }

        {
            ksync::lock!(let _guard = target.lock_lock());
            target.unregister_interrupts();
        }
        drop(target);
    }

    /// Creates a new [`SmmuBti`] for `bus_txn_id`.
    pub fn create_bti(&self, bus_txn_id: u64) -> Result<RefPtr<SmmuBti>, Status> {
        // Start by making sure the mask/id being passed to us is supported.
        let stream_ids = SmrValue::from_bti_id(bus_txn_id, self.valid_stream_id_mask())?;

        // Next, make sure that this mask/id does not collide with any Stream IDs
        // owned by existing BTIs.
        ksync::lock!(let mut guard = self.lock_lock());

        for bti in guard.bti_list().iter() {
            if bti.smr_intersects(stream_ids) {
                return Err(Status::ALREADY_EXISTS);
            }
        }

        // Great! It looks like this is a new set of stream IDs. We'll need to
        // allocate an SMRG and a ContextBank in order to proceed. See if we have any
        // to allocate.
        let smrg_ndx = guard.available_smrgs().find_first_set_bit().ok_or(Status::NO_RESOURCES)?;
        let cb_ndx = guard.available_cbs().find_first_set_bit().ok_or(Status::NO_RESOURCES)?;

        // Looks like we have the resources available, construct objects to manage them.
        let params = self.controller_params(&guard);
        let smrg = StreamMatchRegGroup::create_and_lockdown(&params, smrg_ndx, stream_ids)?;
        let cb = match ContextBank::create_and_lockdown(&params, cb_ndx) {
            Ok(cb) => cb,
            Err(err) => {
                // Temporary porting note: the C++ `CreateBti` simply returned here, destroying an
                // SMRG whose register bookkeeping had not been reset, which tripped
                // `~StreamMatchRegGroup`'s debug asserts. Explicitly disable it first.
                //
                // What the hardware sees which it did not with the C++: one more pass of
                // `StreamMatchRegGroup::disable_regs` (SMR read and write with VALID, MASK and
                // ID zero, S2CR read and write with TYPE = Fault, then a barrier). These are the
                // values `create_and_lockdown` wrote a moment ago with `lock` held throughout,
                // so no register changes value and the SMRG stays disabled, as it was left by
                // the C++. This is at least as good: same final state, a few redundant accesses.
                let mut smrg = smrg;
                smrg.disable();
                return Err(err);
            }
        };

        // Almost there. Now we just need to construct a BTI, and activate it
        // according to our current SMMU mode of operation (either passthru, or
        // enforced).
        //
        // If our SMMU driver is set to disabled, no one should be attempting to
        // allocate any BTIs here. They should be using StubIommus instead.
        debug_assert!(
            self.op_mode() == ArmSmmuMode::Passthru || self.op_mode() == ArmSmmuMode::Enforced,
            "SMMU must be passthru or enforced when creating a BTI ({})",
            self.op_mode() as u32
        );
        let bti_mode = if self.op_mode() == ArmSmmuMode::Passthru {
            BtiMode::Bypass
        } else {
            BtiMode::Translation
        };

        let bti = SmmuBti::create(self, &mut guard, smrg, Some(cb), bti_mode)?;

        // Victory! Add ourselves to the list of active BTIs, and return a reference
        // for the BTI dispatcher to hold on to.
        guard.as_mut().bti_list_mut().push_back(bti.clone());
        Ok(bti)
    }

    fn init(&self, config: &ZbiDcfgArmSmmuDriver) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.lock_lock());

        // First, find our register bank in the peripheral map and stash our virtual
        // address to the registers. Then initialize our MMIO base for our global
        // registers.
        //
        // TODO(johngro): this code currently assumes that if the first address
        // exists, that all of the SMMU pages exist in the peripheral map. Some day
        // we should:
        // 1) Extend the peripheral map API to allow us to translate an address while
        //    simultaneously confirming that at least X bytes exist in the map after
        //    the translated address.
        // 2) Start by asking for a single page. Use the ID registers to determine
        //    the total size of the SMMU register bank.
        // 3) Then compute how many context banks are available to us, and have not
        //    been hidden by the secure side of things.
        // 4) Finally, verify that we can access the subset of register pages that we
        //    are supposed to be able to access.
        let reg_vaddr = SmmuEnv::periph_paddr_to_vaddr(self.phys_reg_base);
        if reg_vaddr == 0 {
            return Err(Status::NO_RESOURCES);
        }
        // SAFETY: `reg_vaddr` is the peripheral-map virtual address of `self.phys_reg_base`.
        let gr0_base = unsafe { SmmuMmio::new(reg_vaddr) };

        // Read and cache our ID registers. Check the version number to be sure we
        // recognize the hardware. Stash our initial count of context banks and
        // compute our friendly name as we go.
        let idr0 = Idr0::read_from(&gr0_base);
        let idr1 = Idr1::read_from(&gr0_base);
        let idr2 = Idr2::read_from(&gr0_base);
        let idr7 = Idr7::read_from(&gr0_base);
        let (name_buf, name_len) = Self::format_smmu_name(self.phys_reg_base);

        // SAFETY: `self` has not been published yet and we hold `self.lock`.
        unsafe {
            let st = &mut *self.init_state.get();
            st.name_buf = name_buf;
            st.name_len = name_len;
            st.gr0_base = gr0_base;
            st.idr0 = idr0;
            st.idr1 = idr1;
            st.idr2 = idr2;
            st.idr7 = idr7;
        }

        if idr7.major() != 2 {
            dprintf!(
                INFO,
                "{} (v{}.{}) is not supported\n",
                self.name(),
                idr7.major(),
                idr7.minor()
            );
            return Err(Status::INVALID_ARGS);
        } else {
            dprintf!(INFO, "Found {} (v{}.{})\n", self.name(), idr7.major(), idr7.minor());
        }

        // Figure out how many context banks and SMRs we are allowed to use. This is
        // based on how many are still available to us after the secure side of things
        // has taken theirs, as well as the artificial limit which might be passed to
        // us by configuration.
        let mut num_cbs = idr1.numcb();
        if config.num_context_banks_override != 0 && config.num_context_banks_override < num_cbs {
            num_cbs = config.num_context_banks_override;
        }

        let mut num_smrgs = idr0.numsmrg();
        if config.num_smr_override != 0 && config.num_smr_override < num_smrgs {
            num_smrgs = config.num_smr_override;
        }

        // Compute the base address of the other register banks we plan to use. See
        // Section 8.1 for the details of the register address space.
        let page_size = self.reg_page_size();
        let reg_size = page_size * self.reg_page_cnt();
        // SAFETY: `reg_vaddr` is the peripheral-map virtual address of `self.phys_reg_base`, and
        // `[reg_vaddr, reg_vaddr + 2 * reg_size)` is the SMMUv2 global and context register space.
        let (gr0_base, gr1_base, cb0_base) = unsafe {
            (
                SmmuMmio::with_size(reg_vaddr, page_size),
                SmmuMmio::with_size(reg_vaddr + page_size, page_size),
                SmmuMmio::with_size(reg_vaddr + reg_size, reg_size),
            )
        };

        // Figure out what our operating mode should be based on our address and the
        // Zircon boot option.
        let mut op_mode = SmmuEnv::boot_option_smmu_mode(Some(self.phys_reg_base));

        // If "Enforced" has been selected, but this SMMU instance does not support
        // translation, we will need to downgrade to Bypass mode.
        if op_mode == ArmSmmuMode::Enforced && idr0.s1ts() == 0 {
            dprintf!(
                INFO,
                "WARNING - {} does not support Stage 1 Translation, but fully enforced operation was requested.  Downgrading to passthru mode.\n",
                self.name()
            );
            op_mode = ArmSmmuMode::Passthru;
        }

        // This code currently depends on stream-matching support and does not support
        // the older stream indexing mode.
        if idr0.sms() == 0 {
            dprintf!(
                INFO,
                "ERROR - {} does not support stream-matching (IDR0 {:#010x})\n",
                self.name(),
                idr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // This code does not currently support extended stream IDs. When extended
        // stream IDs are supported and enabled, the match registers' ID and MASK
        // fields move from being 15 bits, to 16 bits, and the location of the "valid"
        // bit moves from the SMR (stream match register) to the S2CB (stream to
        // context bank) register. Currently, if the feature is enabled and supported
        // when we come out of the bootloader, we _assume_ that it is required to
        // support the stream ID space used by the SOC, so we refuse to load.
        //
        // Support for this feature is not a huge lift, it just requires abstracting
        // the SMR/S2CB interface just a little bit.
        //
        // See section 2.3.1 Transaction streams : StreamID size.
        let mut cr0 = Cr0::read_from(&gr0_base);
        if idr0.exids() != 0 && cr0.exidenable() == ExidEnable::Enabled {
            dprintf!(
                INFO,
                "ERROR - {} does not support extended StreamIDs (IDR0 {:#010x}, CR0 {:#010x})\n",
                self.name(),
                idr0.reg_value(),
                cr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // In SMMUv2, IDR0.NUMIRPT == 1 means that we have a dedicated interrupt pin
        // for every context bank. If the value is > 1, then there are a pool of
        // interrupts which need to be shared across the various context banks, and
        // the CBAR.IRPTNDX field will need to be programmed to map specific context
        // banks to specific interrupts. Currently, we only support the dedicated
        // interrupt model, so make sure that is how this SMMU is configured.
        //
        // See section 3.6.2 Context Bank Interrupts.
        if idr0.numirpt() != 1 {
            dprintf!(
                INFO,
                "ERROR - {} SMMU does not have a dedicated interrupt per context bank. (IDR0 {:#010x})\n",
                self.name(),
                idr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // Use the number of stream ID bits as reported in IDR0 to compute a mask
        // which can tell us if an encoded stream ID ((mask << 16 | id) is valid or
        // not.
        let mut valid_stream_id_mask = (1u32 << idr0.numsidb()) - 1;
        valid_stream_id_mask |= valid_stream_id_mask << 16;

        // Similar to extended StreamIDs, we do not currently support the "Extended
        // Stream Matching" extension, which increases the total number of stream
        // match registers from 128 up to as many as 1024. Adding support would
        // involve changes to the logic of finding and allocating stream match
        // contexts, as well as some additional save/restore code to support SMMU low
        // power states. See Chapter 14 : Extended Stream Matching Extension.
        let mut cr2 = Cr2::read_from(&gr0_base);
        if idr0.exsmrgs() != 0 && cr2.exsmrgenable() != 0 {
            dprintf!(
                INFO,
                "{} does not support extended stream matching (IDR0 {:#010x}, CR2 {:#010x})\n",
                self.name(),
                idr0.reg_value(),
                cr2.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // SAFETY: `self` has not been published yet and we hold `self.lock`.
        unsafe {
            let st = &mut *self.init_state.get();
            st.op_mode = op_mode;
            st.valid_stream_id_mask = valid_stream_id_mask;
            st.num_cbs = AtomicU32::new(num_cbs);
            st.num_smrgs = num_smrgs;
            st.gr0_base = gr0_base;
            st.gr1_base = gr1_base;
            st.cb0_base = cb0_base;
        }

        // Allocate storage for our global and context interrupts based on the values
        // passed to us, and make copies of the configuration.
        //
        // Temporary porting note: the C++ `Init` only debug-asserted that these counts were
        // consistent, and went on to build out-of-bounds spans from them in release builds. A
        // malformed configuration is rejected here instead.
        //
        // What the hardware sees for such a configuration: only the reads of IDR0, IDR1, IDR2,
        // IDR7, CR0 and CR2 above. Nothing is written, no interrupt is configured, and there is
        // no lockdown or TLB invalidation, so the SMMU stays exactly as the bootloader left it.
        // That is what both implementations do for every other configuration they reject
        // before this point (unsupported version, no stream matching, and so on). The C++
        // either panicked (debug asserts on) or carried on with interrupt definitions read from
        // past the end of the array, configuring and registering whatever interrupt numbers it
        // found there, after which what reached the hardware was undefined. A defined refusal
        // is at least as good as that. It does mean a bad IRQ count leaves bootloader stream
        // mappings in place where a C++ release build might, by luck, have gone on to lock
        // them down.
        if (config.irq_cnt as usize) > config.irqs.len() || config.global_irq_cnt > config.irq_cnt {
            dprintf!(
                INFO,
                "ERROR - {} invalid IRQ counts (irq_cnt={}, global_irq_cnt={})\n",
                self.name(),
                config.irq_cnt,
                config.global_irq_cnt
            );
            return Err(Status::INVALID_ARGS);
        }
        debug_assert!((config.irq_cnt as usize) <= config.irqs.len());
        debug_assert!(config.global_irq_cnt <= config.irq_cnt);
        let all_irqs = &config.irqs[..config.irq_cnt as usize];
        let global_irqs = &all_irqs[..config.global_irq_cnt as usize];
        let context_irqs = &all_irqs[config.global_irq_cnt as usize..];

        self.allocate_global_irqs(global_irqs)?;
        self.allocate_context_irqs(context_irqs, idr1.numcb())?;

        // Set up our "available" bit masks. At this point, all SMRGs and CBs in the
        // system are considered to be "available", however right after this we might
        // end up reserving some of them because they were flagged as "Early init"
        // contexts.
        guard.as_mut().available_smrgs_mut().set_lowest_n_bits(self.num_smrgs());
        guard.as_mut().available_cbs_mut().set_lowest_n_bits(self.num_cbs());

        // If our mode is "disabled", don't go any further. We don't want to actually
        // claim any hardware, or change any of the existing HW configuration.
        if op_mode == ArmSmmuMode::Disabled {
            return Ok(());
        }

        // Reserve our registers so that they cannot be accessed by user mode.
        SmmuEnv::root_resource_filter_add_deny_region(
            self.phys_reg_base,
            reg_size << 1,
            ZX_RSRC_KIND_MMIO,
        );

        // If we are not disabled, make sure our configuration registers are set up as
        // we want them to be.
        cr0.set_vmid16en(0) // 8-bit VMIDs
            .set_smcfcfg(1) // Raise a fault when there are stream-match conflicts.
            .set_fb(0) // Disable force broadcast of TLB/cache maintenance ops.
            .set_ptm(1) // Private TLB maintenance. Don't listen to system-wide broadcasts.
            .set_vmidpne(1) // Private VMID namespaces
            .set_usfcfg(1) // Raise a fault on unidentified stream access.
            .set_gse(0) // Disable global stalling across contexts.
            .set_stalld(Stalld::Disabled) // Disable per-context stalling.
            .set_gcfgfie(0) // Disable global config fault interrupts (enabled below)
            .set_gcfgfre(1) // Abort transactions during global config faults (not RAZ/WI).
            .set_exidenable(ExidEnable::Disabled) // Do not use extended IDs (we checked this above)
            .set_gfie(0) // Disable global fault interrupts (enabled below)
            .set_gfre(1) // Abort transactions during global faults (not RAZ/WI).
            // Accesses are subject to translation, access ctrl, and attr-gen.
            .set_clientpd(ClientPd::Enabled)
            .write_to(&gr0_base);

        cr2.set_exsmrgenable(0) // Extended SMRGs are disabled.
            .set_compindexenable(0) // Disabled compressed StreamID matching (eg; use the SMRs)
            .write_to(&gr0_base);

        // Register all of our interrupts. If anything goes wrong after this, be sure
        // to give them all back.
        let mut cleanup_irqs = zr::defer(|| self.unregister_interrupts());
        self.register_global_irqs()?;
        self.register_context_irqs()?;

        // Go over the list of "handoff stream IDs" passed to us and adopt all of the
        // associated stream match register groups and context banks so that they are
        // not automatically locked down, or claimed by user-mode.
        //
        // Temporary porting note: the C++ `Init` used `config.handoff_smr_cnt` unchecked. The
        // count is clamped to the size of the array here.
        //
        // For a count within the array the register accesses are identical. For a larger one
        // the C++ read past the end of `handoff_smrs` and treated whatever it found as stream
        // IDs; any which happened to intersect a valid SMR caused that SMRG to be adopted, and
        // so left enabled instead of locked down. With the clamp, only stream IDs which are
        // really in the configuration can exempt an SMRG from lockdown, which is at least as
        // good: everything else is locked down, as it should be.
        let handoff_cnt = (config.handoff_smr_cnt as usize).min(config.handoff_smrs.len());
        let handoff_stream_ids = &config.handoff_smrs[..handoff_cnt];
        for &sid in handoff_stream_ids {
            if (sid & !valid_stream_id_mask) != 0 {
                dprintf!(
                    INFO,
                    "WARNING: {} is ignoring invalid early init stream ID {:#010x}\n",
                    self.name(),
                    sid
                );
                continue;
            }

            let early_stream_ids = SmrValue::from_raw(sid, valid_stream_id_mask);
            for i in 0..self.num_smrgs() {
                let smr = Smr::read_at(&gr0_base, i);

                // When attempting to decide whether or not to adopt an SMRG, we compute
                // the intersection of the SMR mask/value with each member of the
                // early-init vector passed to us by config. Theoretically, this lets a
                // configuration specify multiple SIDs to adopt using only a single entry.
                //
                // That said, there are many possible misconfigurations for which proper
                // behavior is not well defined. For example, if pre-existing hardware
                // has a set of SID `[0x1, 0x3]`, and the global set of SIDs to adopt
                // includes `[0x1, 0x4, 0x5]`, then we have encountered an SMRG where one
                // of its SIDs is supposed to be adopted, and another is not.
                //
                // For now, we do our best with our intersection test. There is no
                // particularly clear guidance on how this is formally supposed to work.
                // Furthermore, we assume that if there is something incorrect about this
                // approach, that it will become apparent early in platform bringup, and
                // someone can come back here to tweak the code to handle the newly
                // discovered quirk.
                let reg_stream_ids = SmrValue::from_smr_reg(smr, valid_stream_id_mask);
                if smr.valid() != 0 && early_stream_ids.intersects(reg_stream_ids) {
                    match self.adopt_smrg(&mut guard, i, reg_stream_ids) {
                        Ok(()) => {
                            dprintf!(
                                INFO,
                                "{} : Locked SMRG {} (MASK {:#06x}, ID {:#06x}) which intersects early-init stream ID (MASK {:#06x}, ID {:#06x})\n",
                                self.name(),
                                i,
                                smr.mask(),
                                smr.id(),
                                early_stream_ids.mask(),
                                early_stream_ids.id()
                            );
                        }
                        Err(status) => {
                            dprintf!(
                                INFO,
                                "{} : ERROR - Failed to lock SMRG {} (MASK {:#06x}, ID {:#06x}) (err = {})\n",
                                self.name(),
                                i,
                                smr.mask(),
                                smr.id(),
                                status.into_raw()
                            );
                        }
                    }
                }
            }
        }

        // Now, lock-down any contexts which were not adopted.
        self.lockdown(&guard);

        // If the number of context banks we have is larger than the number of
        // context bank interrupts we were given, artificially limit the number of
        // context banks we will make available and use. We don't want to use any
        // context banks that we don't have interrupts for.
        let context_irq_cnt = context_irqs.len() as u32;
        if self.num_cbs() > context_irq_cnt {
            for i in context_irq_cnt..self.num_cbs() {
                guard.as_mut().available_cbs_mut().clear_bit(i);
            }
            // `Relaxed` suffices: written while holding `self.lock` before `self` is published
            // under `SmmuInstanceLock` or any context IRQ is unmasked; `AtomicU32` only avoids forming
            // `&mut SmmuInitState` after IRQ handlers are registered.
            self.init_state().num_cbs.store(context_irq_cnt, Ordering::Relaxed);
        }

        // Now that everything has been either locked down or adopted, globally
        // invalidate all of the non-secure, non-hypervisor TLB entries, providing us
        // a well defined starting state for the TLB cache.
        self.invalidate_all_tlb_entries();

        // Finally, enable global interrupts.
        let mut unmask_err: Option<Status> = None;
        {
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            for vec in irq_guard.global_irqs().iter() {
                if vec.registered
                    && let Err(status) = SmmuEnv::unmask_interrupt(vec.irq_def.num)
                {
                    dprintf!(
                        INFO,
                        "{} : Failed to unmask global IRQ {:#04x}\n",
                        self.name(),
                        vec.irq_def.num
                    );
                    unmask_err = Some(status);
                    break;
                }
            }

            // Enable global fault interrupts while holding `irq_lock`.
            if unmask_err.is_none() {
                cr0.set_gcfgfie(1).set_gfie(1).write_to(&gr0_base);
            }
        }
        if let Some(status) = unmask_err {
            return Err(status);
        }

        // Success. Cancel any cleanup operations and we are done.
        cleanup_irqs.cancel();
        Ok(())
    }

    fn allocate_global_irqs(&self, src: &[ZbiDcfgArmSmmuIrq]) -> Result<(), Status> {
        if src.is_empty() {
            return Ok(());
        }
        let num_vectors = src.len();
        let mut arr = Array::<GlobalIrqVector>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Global IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        let mut cookies = Array::<IrqCookie>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Global IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        for (i, item) in src.iter().enumerate() {
            arr[i] = GlobalIrqVector { irq_def: IrqDef::from_zbi(*item), registered: false };
            cookies[i] = IrqCookie { smmu: self as *const Smmu, ndx: i as u32 };
        }

        // SAFETY: `self` has not been published yet, we hold `self.lock`, and no interrupt
        // handler has been registered, so nothing else can be reading the cookies.
        unsafe {
            *self.global_irq_cookies.get() = cookies;
        }

        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        *irq_guard.as_mut().global_irqs_mut() = arr;
        Ok(())
    }

    fn allocate_context_irqs(&self, src: &[ZbiDcfgArmSmmuIrq], numcb: u32) -> Result<(), Status> {
        if src.is_empty() {
            return Ok(());
        }
        let num_vectors = (src.len() as u32).min(numcb) as usize;
        let mut arr = Array::<ContextIrqVector>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Context IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        let mut cookies = Array::<IrqCookie>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Context IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        for (i, item) in src.iter().take(num_vectors).enumerate() {
            arr[i] = ContextIrqVector {
                irq_def: IrqDef::from_zbi(*item),
                enabled: false,
                registered: false,
                in_flight: false,
                bti: None,
            };
            cookies[i] = IrqCookie { smmu: self as *const Smmu, ndx: i as u32 };
        }

        // SAFETY: `self` has not been published yet, we hold `self.lock`, and no interrupt
        // handler has been registered, so nothing else can be reading the cookies.
        unsafe {
            *self.context_irq_cookies.get() = cookies;
        }

        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        *irq_guard.as_mut().context_irqs_mut() = arr;
        Ok(())
    }

    fn register_irqs<V: IrqVectorEntry>(&self) -> Result<(), Status> {
        let cookies = V::cookies(self);
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        let len = V::slice(&irq_guard).len();

        for i in 0..len {
            // Validate, configure, and mask the interrupt with `irq_lock` held (interrupts off),
            // dropping `irq_lock` only around `register_int_handler` below.
            let def = V::slice(&irq_guard)[i].irq_def();
            // SAFETY: `cookies` is immutable after `allocate_*_irqs`, which sized it to match the
            // corresponding `*_irqs` slice.
            let v_ptr = unsafe { (&raw const (&*cookies.get())[i]) }.cast_mut().cast::<c_void>();

            if !def.valid() {
                dprintf!(
                    INFO,
                    "{} : Invalid IRQ flags ({:#04x}) for {} IRQ num {:#04x}\n",
                    self.name(),
                    def.flags,
                    V::KIND,
                    def.num
                );
                return Err(Status::INVALID_ARGS);
            }

            if let Err(status) =
                SmmuEnv::configure_interrupt(def.num, def.trigger_mode(), def.polarity())
            {
                dprintf!(
                    INFO,
                    "{} : Failed to configure {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            if let Err(status) = SmmuEnv::mask_interrupt(def.num) {
                dprintf!(
                    INFO,
                    "{} : Failed to mask {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            // `irq_lock` is dropped before registering our interrupt handler.
            //
            // The interrupt drivers have a central lock (call it `A`) which is held during calls
            // to register/unregister, and also during dispatch before calling the registered
            // callback, which acquires `irq_lock` (after `A`). Registering with `irq_lock` held
            // would acquire them in the opposite order. This cannot actually deadlock, since the
            // interrupt was masked above and cannot fire, but tools like lockdep would see the
            // potential for it.
            //
            // SAFETY: `v_ptr` points to `cookies[i]`, which remains valid until
            // `unregister_interrupts` completes.
            if let Err(status) = irq_guard.as_mut().call_unlocked(|| unsafe {
                SmmuEnv::register_int_handler(def.num, v_ptr, V::THUNK)
            }) {
                dprintf!(
                    INFO,
                    "{} : Failed to register {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            V::slice_mut(irq_guard.as_mut())[i].set_registered(true);
        }

        Ok(())
    }

    fn register_global_irqs(&self) -> Result<(), Status> {
        self.register_irqs::<GlobalIrqVector>()
    }

    fn register_context_irqs(&self) -> Result<(), Status> {
        self.register_irqs::<ContextIrqVector>()
    }

    fn deactivate_irqs<V: IrqVectorEntry>(&self, irq_guard: &mut Pin<&mut SmmuIrqLockGuard<'_>>) {
        let len = V::slice(irq_guard).len();
        for i in 0..len {
            let maybe_def = {
                let v = &V::slice(irq_guard)[i];
                v.registered().then(|| v.irq_def())
            };
            if let Some(def) = maybe_def {
                if let Err(err) =
                    irq_guard.as_mut().call_unlocked(|| SmmuEnv::unregister_int_handler(def.num))
                {
                    panic!(
                        "{} : Failed to unregister {} IRQ num {:#04x} (flags {:#04x}, err {}) during shutdown.",
                        self.name(),
                        V::KIND,
                        def.num,
                        def.flags,
                        err.into_raw()
                    );
                }
                V::slice_mut(irq_guard.as_mut())[i].set_registered(false);
            }
        }
    }

    fn unregister_interrupts(&self) {
        // Unregister our interrupts with `irq_lock` dropped around each `unregister_int_handler`
        // call. Why do we do this and why is this safe?
        //
        // When interrupts are dispatched, there is currently a global lock (the `pdev_lock`)
        // which is held for the duration of the dispatch of any non-permanent interrupt handler.
        // This lock is also held during registration/unregistration of an interrupt.
        //
        // Holding the lock in these two places provides a guarantee that after calling
        // `unregister_int_handler(num)`, we can be certain that we are not racing with any
        // interrupts in flight, and it is now safe to destroy any resources that an IRQ handler
        // might be targeting.
        //
        // But, it also implies that we have lock ordering rules to follow. We hold `irq_lock`
        // both while we dispatch interrupts, as well as when we modify our internal interrupt
        // state (registering/un-registering interrupts). The IRQ handler is going to obtain the
        // `pdev_lock`, and then attempt to hold our `irq_lock`. If we hold `irq_lock` while
        // walking our table, and then call `unregister_int_handler` with the lock held, it will
        // attempt to hold `pdev_lock` and we will have set up the potential for deadlock.
        //
        // So, we drop `irq_lock` while we unregister. Why is this ok? There are only two types of
        // places that we hold `irq_lock` in this code: during dispatch, where we hold only
        // `irq_lock`, and when creating/destroying associations between context interrupts and
        // `SmmuBti`s, or when re-enabling a context bank interrupt after a fault. In each of
        // these situations, we also hold the top-level `lock` mutex.
        //
        // The IRQ handler is not going to mutate our interrupt tables, so it is fine if there is
        // an IRQ in flight for us to drop `irq_lock` here and un-register the interrupt. Doing so
        // synchronizes with the interrupt dispatch, meaning it is safe to re-lock and modify the
        // table after we have un-registered. For all BTI-related operations, holding the main
        // `lock` during `unregister_interrupts` is what protects us.
        //
        // TODO(johngro): consider modeling this more formally using a token and a two-lock
        // AssertHeld pattern. Gaining exclusive access to the token requires holding both `lock`
        // and `irq_lock`, while gaining shared access requires holding only one of the two.
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        self.deactivate_irqs::<GlobalIrqVector>(&mut irq_guard);
        self.deactivate_irqs::<ContextIrqVector>(&mut irq_guard);
    }

    fn handle_global_irq(&self, ndx: u32) {
        {
            // Mask the top level interrupt to prevent an unknown device from spamming
            // the global IRQ handler with interrupts.
            //
            // TODO(johngro): Figure out a better approach to this.
            //
            // 1) If there is a device we have never created a BTI for which is getting
            //    blocked by us, we'd like to know about it, but we need a way to stop
            //    the device from spamming the global IRQ handler with faults. Look
            //    into either force-stalling the device, or perhaps disabling global
            //    _fault_ interrupts (but not global config error interrupts) and
            //    re-enabling them later on using a timer or a DPC.
            // 2) If we receive a global config interrupt, consider panicking. There is
            //    no good reason we should ever have a bad configuration, and we should
            //    consider this to be a bug.
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            debug_assert!((ndx as usize) < irq_guard.global_irqs().len());
            let _ = SmmuEnv::mask_interrupt(irq_guard.global_irqs()[ndx as usize].irq_def.num);
        }

        // Start by reading the global fault status register to see what caused the
        // interrupt. A value of zero means that we have processed everything and
        // are finished.
        let gr0_base = self.gr0_base();
        let mut gfsr = Gfsr::read_from(&gr0_base);
        if gfsr.reg_value() == 0 {
            return;
        }

        // If the MULTI bit is set, it means that there was at least one more fault
        // which occurred before we were able to service the first fault. When this
        // happens, the MULTI bit is set, but no other details of the fault are
        // recorded in order to preserve the values in the fault registers which
        // describe the first fault.
        if gfsr.multi() != 0 {
            dprintf!(
                INFO,
                "{} : FAULT - At least one global fault occurred while an existing fault was pending.  Details of subsequent faults have been lost.\n",
                self.name()
            );
        }

        const FAULT_NAMES: [&str; 9] = [
            "Invalid context",                  // bit 0, ICF
            "Unidentified stream",              // bit 1, USF
            "Stream match conflict",            // bit 2, SMCF
            "Unimplemented context bank",       // bit 3, UCBF
            "Unimplemented context interrupt",  // bit 4, UCIF
            "Configuration access",             // bit 5, CAF
            "External",                         // bit 6, EF
            "Permission",                       // bit 7, PF
            "Unsupported upstream transaction", // bit 8, UUT
        ];

        let gfar = Gfar::read_from(&gr0_base);
        let gfsynr0 = Gfsynr0::read_from(&gr0_base);
        let gfsynr1 = Gfsynr1::read_from(&gr0_base);

        let fault_ndx = gfsr.reg_value().trailing_zeros() as usize;
        let fault_name = FAULT_NAMES.get(fault_ndx).copied().unwrap_or("Unknown");
        dprintf!(
            INFO,
            "{}: A global \"{}\" fault has occurred. (GFSR = {:#010x})\n",
            self.name(),
            fault_name,
            gfsr.reg_value()
        );
        dprintf!(INFO, "{}: Fault Address : {:#018x}\n", self.name(), gfar.faddr());
        dprintf!(INFO, "{}: Stream ID     : {:#06x}\n", self.name(), gfsynr1.stream_id());
        dprintf!(
            INFO,
            "{}: {}{} {} {} {}fault (GFSYNR0 = {:#010x})\n",
            self.name(),
            if gfsynr0.nested() != 0 { "Nested, " } else { "" },
            if gfsynr0.pnu() != 0 { "Privileged" } else { "Non-Privileged" },
            if gfsynr0.ind() != 0 { "instruction" } else { "data" },
            if gfsynr0.wnr() != 0 { "write" } else { "read" },
            if gfsynr0.ats() != 0 { "address translation " } else { "" },
            gfsynr0.reg_value()
        );

        // Zero all of the fault-detail registers, then ack the fault(s) in the status
        // register (GFSR) by writing back the values we read from it at the start of
        // processing. See section 9.6.15 which says "A value of 1 written to any
        // non-reserved bit clears that bit".
        Gfar::from_value(0).write_to(&gr0_base);
        Gfsynr0::from_value(0).write_to(&gr0_base);
        // Temporary porting note: the C++ `HandleGlobalIrq` wrote `Gfsynr0` twice instead of
        // clearing `Gfsynr1`; preserved for 1:1 parity here and fixed in a follow-up commit.
        Gfsynr0::from_value(0).write_to(&gr0_base);
        gfsr.write_to(&gr0_base);
    }

    fn handle_context_irq(&self, cb_ndx: u32) {
        debug_assert!(cb_ndx < self.num_cbs());

        let (target, irq_num) = {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            debug_assert!((cb_ndx as usize) < irq_guard.context_irqs().len());
            let vec = &mut irq_guard.as_mut().context_irqs_mut()[cb_ndx as usize];

            // Attempt to take a reference to our target BTI before we drop the IRQ lock
            // and dispatch the interrupt.
            let irq_num = vec.irq_def.num;
            let target = vec.bti.clone();
            let _ = SmmuEnv::mask_interrupt(irq_num);
            if target.is_some() {
                vec.in_flight = true;
                vec.enabled = false;
            }
            (target, irq_num)
        };

        if let Some(ref bti) = target {
            // Enter our target's lock, print our fault details, then inform our BTI of
            // the fault so it can go into lockdown mode and (someday) raise a signal on
            // the kernel object indicating that there was a fault.
            //
            // The only way to restore functionality at this point is for the driver to
            // take control of its hardware, and release the quarantine state on the BTI.
            ksync::lock!(let mut bti_guard = bti.lock_lock());
            let name_buf = *bti_guard.name();
            let bti_name = SmmuBti::name_display(&name_buf);
            self.print_common_context_irq_fault_details(cb_ndx, Some(bti_name));
            bti.handle_fault_locked(&mut bti_guard);
        } else {
            // This is very strange, but it is technically possible to receive a CB
            // interrupt with no target if we happen to be shutting down our target
            // right as one last fault interrupt arrives. In a situation like this, we
            // (the SMMU instance) _technically_ own these context bank fault syndrome
            // registers, so it should be OK to access the registers without holding a
            // BTI's lock.
            dprintf!(
                INFO,
                "{} : WARNING - received context IRQ ({:#04x}) for context bank {}, but no BTI is registered.\n",
                self.name(),
                irq_num,
                cb_ndx
            );
            self.print_common_context_irq_fault_details(cb_ndx, None);
        }

        // Clear our `in_flight` flag if we set it above.
        //
        // Temporary porting note: the C++ `HandleContextIrq` cleared the flag from an
        // unconditional deferred action which debug-asserted `vec.in_flight == true`, even though
        // the flag is only set when a target BTI was found. A context IRQ arriving with no BTI
        // registered therefore tripped that assert in C++ debug builds. Here the flag is only
        // checked and cleared when it was set.
        if target.is_some() {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            let vec = &mut irq_guard.as_mut().context_irqs_mut()[cb_ndx as usize];
            debug_assert!(vec.in_flight);
            vec.in_flight = false;
        }
    }

    fn print_common_context_irq_fault_details(
        &self,
        cb_ndx: u32,
        bti_name: Option<EscapedName<'_>>,
    ) {
        // Unconditionally disable the context bank interrupt at the context bank level.
        let cb_base = self.get_cb_base(cb_ndx);
        let gr1_base = self.gr1_base();
        Sctlr::read_from(&cb_base).set_cfie(0).write_to(&cb_base);

        // Next attempt to log the relevant details of the fault, even if there is no
        // user-mode BTI associated with this context bank.
        let mut fsr = Fsr::read_from(&cb_base);
        let far = Far::read_from(&cb_base);
        let fsynr0 = Fsynr0::read_from(&cb_base);
        let cbfrsynra = Cbfrsynra::read_at(&gr1_base, cb_ndx);

        const FAULT_NAMES: [&str; 9] = [
            "Unknown (reserved)",               // bit 0, Reserved
            "Translation",                      // bit 1, TF
            "Access flag",                      // bit 2, AFF
            "Permission",                       // bit 3, PF
            "External",                         // bit 4, EF
            "TLB match conflict",               // bit 5, TLBMCF
            "TLB lock",                         // bit 6, TLBLKF
            "Address size",                     // bit 7, ASF
            "Unsupported upstream transaction", // bit 8, UUT
        ];
        let fault_ndx = fsr.reg_value().trailing_zeros() as usize;
        let fault_name = FAULT_NAMES.get(fault_ndx).copied().unwrap_or("Unknown");

        {
            // Render our report into the global fault buffer while holding the global fault buffer
            // lock, then emit it with a single console write so that the lines of one report
            // cannot be interleaved with other output. Sharing a global fault buffer allows us to
            // render the string without needing a large stack allocation at hard IRQ time.
            //
            // It is assumed that systems will not be taking large numbers of concurrent SMMU
            // faults, meaning that this lock should never be anything resembling a significant
            // choke point.
            ksync::lock!(let mut fault_guard = FaultBufferLock::lock());
            let mut f =
                FaultReportWriter::new(FAULT_BUFFER.get_mut(fault_guard.as_mut().token_mut()));

            let yes_no = |flag: u32| if flag != 0 { "yes" } else { "no" };
            let _ = writeln!(
                f,
                "{}{} \"{}\" fault has occurred in context bank {}{}. (FSR = {:#010x})",
                DRIVER_OOPS_BANNER,
                if fsr.multi() != 0 { "More than one" } else { "A" },
                fault_name,
                cb_ndx,
                if fsr.ss() != 0 { ", and the context is now stalled" } else { "" },
                fsr.reg_value()
            );
            let _ = writeln!(f, "SMMU              : {}", self.name());
            let _ = writeln!(
                f,
                "BTI Name          : {}",
                bti_name.unwrap_or(EscapedName::new(b"<unknown>"))
            );
            let _ = writeln!(f, "Stream ID         : {:#x}", cbfrsynra.stream_id());
            let _ = writeln!(f, "Fault Address     : {:#018x}", far.faddr());
            let _ = writeln!(f, "Stalled           : {}", yes_no(fsr.ss()));
            let _ = writeln!(f, "Permission Fault  : {}", yes_no(fsr.pf()));
            let _ = writeln!(f, "Access Fault      : {}", yes_no(fsr.aff()));
            let _ = writeln!(f, "Translation Fault : {}", yes_no(fsr.tf()));
            const OTHER_FAULT_MASK: u32 = 0x1f0;
            if (fsr.reg_value() & OTHER_FAULT_MASK) != 0 {
                let _ = writeln!(f, "Other faults present (FSR = {:#010x})", fsr.reg_value());
            }

            if fsr.pf() != 0 || fsr.aff() != 0 || fsr.tf() != 0 {
                let _ = writeln!(
                    f,
                    "{}{} {} {} fault{} at page table level {} (FSYNR0 = {:#010x})",
                    if fsynr0.afr() != 0 { "Asynchronous, " } else { "" },
                    if fsynr0.pnu() != 0 { "Privileged" } else { "Non-Privileged" },
                    if fsynr0.ind() != 0 { "instruction" } else { "data" },
                    if fsynr0.wnr() != 0 { "write" } else { "read" },
                    if fsynr0.ptwf() != 0 { ", during a page-table walk," } else { "" },
                    fsynr0.plvl(),
                    fsynr0.reg_value()
                );
            }

            SmmuEnv::print_driver_oops_report(f.as_str());
        }

        // Clear any of the fault bookkeeping so we are ready to receive a new context
        // fault when we are able to.
        //
        // Then ack the fault(s) in the status register (FSR) by writing back the
        // values we read from it at the start of processing. See section 16.5.9
        // which says "A value of 1 written to any non-reserved bit clears that bit".
        Cbfrsynra::from_value(0).write_at(&gr1_base, cb_ndx);
        Far::from_value(0).write_to(&cb_base);
        Fsynr0::from_value(0).write_to(&cb_base);
        fsr.write_to(&cb_base);
    }

    fn lockdown(&self, guard: &SmmuLockGuard<'_>) {
        let gr0_base = self.gr0_base();
        let gr1_base = self.gr1_base();
        let params = self.controller_params(guard);

        // Go over all of the stream match register groups and make sure that they are
        // all disabled. If we find any enabled SMRs, print some details about the
        // fact that they were enabled as we disabled them.
        for i in 0..self.num_smrgs() {
            // Skip any SMRGs which were adopted during early-init.
            if !guard.available_smrgs().test_bit(i) {
                dprintf!(INFO, "{} : Lockdown skipping adopted SMRG {}\n", self.name(), i);
                continue;
            }

            let smr = Smr::read_at(&gr0_base, i);
            let s2cr = S2cr::read_at(&gr0_base, i);

            if smr.valid() != 0 {
                // Figure out what mode this group is operating in so we can report it.
                let mut tag1 = arm_s2cr_type_to_string(s2cr.s2cr_type());
                let mut tag2 = "";

                // If we are in translation mode, examine the configured context bank to
                // figure out how it was configured instead of simply saying "Translation".
                if s2cr.s2cr_type() == S2crType::Translation {
                    let cb_ndx = S2crTranslation::from_value(s2cr.reg_value()).cbndx();
                    if cb_ndx < self.num_cbs() {
                        tag1 =
                            arm_cbar_type_to_string(Cbar::read_at(&gr1_base, cb_ndx).cbar_type());
                        let cb_base = self.get_cb_base(cb_ndx);

                        // We know that this is the stage 1 version of SCTLR because we know
                        // that this context bank was not adopted, and because we never
                        // currently configure a context bank for stage 2 translation.
                        tag2 = if Sctlr::read_from(&cb_base).m() != 0 {
                            " - S1 Translate Enabled"
                        } else {
                            " - S1 Translate Disabled"
                        };
                    } else {
                        tag1 = "Translate (invalid context bank)";
                    }
                }

                dprintf!(INFO, "{} : Lockdown\n", self.name());
                dprintf!(INFO, "SMRG : {}\n", i);
                dprintf!(INFO, "SMR  : ID {:#06x} MASK {:#06x}\n", smr.id(), smr.mask());
                dprintf!(INFO, "Mode : {}{}\n", tag1, tag2);
            }

            StreamMatchRegGroup::disable_at(&params, i);
        }

        // Go over all of our context banks which have not been claimed (adopted) and
        // disable them so that any transaction which might happen to reach it will be
        // denied.
        for i in 0..self.num_cbs() {
            // Skip any Context Banks which were adopted during early-init.
            if !guard.available_cbs().test_bit(i) {
                dprintf!(INFO, "{} : Lockdown skipping locked CB {}\n", self.name(), i);
                continue;
            }

            ContextBank::disable_at(&params, i);
        }
    }

    fn invalidate_all_tlb_entries(&self) {
        let gr0_base = self.gr0_base();
        // Invalidate all TLB entries tagged as Non-secure, Non-hyp.
        Tlbiallnsnh::from_value(0).write_to(&gr0_base);
        Tlbgsync::from_value(0).write_to(&gr0_base);
        while Tlbgstatus::read_from(&gr0_base).gsactive() != 0 {
            SmmuEnv::yield_cpu();
        }

        // See Section 5.4.2 "TLB maintenance operation processing"'s SYNC assembly
        // example. We need a `dsb sy` instruction following this operation.
        SmmuEnv::device_memory_barrier();
    }

    /// Shuts down `bti` and removes it from this SMMU's BTI list.
    pub(crate) fn shutdown_bti(&self, bti: &SmmuBti) {
        // Shutting a BTI down is a process which takes a bit of care. The sequence
        // is generally as follows:
        //
        // First, lock the SMMU for the duration of the process.
        //
        // Next, we need to deal with any context bank IRQ we may have registered, and
        // be certain that there cannot be any context bank IRQ in flight before we go
        // any further.
        //
        // So, lock the target BTI and peek at its current context bank, if any. If
        // it has a context bank, then make a note of the CB's index. Once a BTI has
        // a context bank assigned, the only way for the context bank (and its
        // associated IRQ) to be returned to the SMMU pool is to be shut down via the
        // SMMU itself. We know that a concurrent shutdown operation cannot take
        // place right now, because we are holding the main SMMU lock, so we can be
        // sure that the CB effectively belongs to us and cannot be reused until we
        // are done here.
        //
        // So, if we have a context bank index assigned to us, we can unregister the
        // IRQ associated with that index (obtaining the IRQ lock in the process) and
        // synchronize with any in-flight IRQ operations without needing to worry
        // about holding the BTI lock as we do.
        //
        // Once we are certain that there are no interrupts in flight targeting our
        // hardware, and that there will not be until we are finished, we are free to
        // call into the BTI instance itself in order to shut down the rest of the
        // hardware.
        let removed = {
            ksync::lock!(let mut guard = self.lock_lock());
            let maybe_cb_ndx = {
                ksync::lock!(let bti_guard = bti.lock_lock());
                bti_guard.context_bank().as_ref().map(|cb| cb.cb_ndx())
            };

            if let Some(cb_ndx) = maybe_cb_ndx {
                // Note: we know that there are at least two additional references to this
                // BTI which currently exist, independent of the reference which might be
                // held by the IRQ handler. They are the reference held by our main BTI
                // list, as well as the reference which this method's caller must be
                // holding.
                self.shutdown_context_bank_irq(cb_ndx);
            }

            // Go ahead and shut down the BTI hardware now, locking down all transactions
            // and returning all resources to the SMMU's pool in the process. Once we are
            // done with that, we can remove ourselves from the global BTI list and we
            // should be finished.
            debug_assert!(bti.in_container());
            bti.shutdown(&mut guard);
            // SAFETY: `bti` is in `self.bti_list`.
            unsafe { guard.as_mut().bti_list_mut().erase(bti) }
        };
        drop(removed);
    }

    /// Associates `bti` with the context bank interrupt at `cb_ndx` and unmasks it.
    pub(crate) fn associate_bti_irq(&self, bti: &RefPtr<SmmuBti>, cb_ndx: u32) {
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        let idx = cb_ndx as usize;
        if idx >= irq_guard.context_irqs().len() {
            dprintf!(
                INFO,
                "{} : WARNING - Failed to associate context IRQ with BTI, invalid index ({} >= {})\n",
                self.name(),
                cb_ndx,
                irq_guard.context_irqs().len()
            );
            return;
        }

        let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
        debug_assert!(vec.bti.is_none());
        debug_assert!(vec.registered);

        vec.bti = Some(bti.clone());
        vec.enabled = true;
        let _ = SmmuEnv::unmask_interrupt(vec.irq_def.num);
    }

    /// Masks and disassociates the context bank interrupt at `cb_ndx`, synchronizing with any
    /// in-flight IRQ.
    pub(crate) fn shutdown_context_bank_irq(&self, cb_ndx: u32) {
        let bti_ref: Option<RefPtr<SmmuBti>>;
        let mut in_flight: bool;
        let idx = cb_ndx as usize;
        {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            if idx >= irq_guard.context_irqs().len() {
                // Temporary porting note: the C++ `ShutdownContextBankIrq` and
                // `ReenableContextBankIrq` had their "re-enable" / "disable" warning messages
                // swapped; preserved for 1:1 parity here and fixed in a follow-up commit.
                dprintf!(
                    INFO,
                    "{} : WARNING - Failed to re-enable context IRQ with BTI, invalid index ({} >= {})\n",
                    self.name(),
                    cb_ndx,
                    irq_guard.context_irqs().len()
                );
                return;
            }

            let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
            debug_assert!(vec.registered);

            in_flight = vec.in_flight;
            bti_ref = vec.bti.take();
            let _ = SmmuEnv::mask_interrupt(vec.irq_def.num);
        }
        drop(bti_ref);

        // Synchronize with any in-flight IRQs. In the unlikely event that an IRQ is
        // currently being processed on another CPU, poll until it completes before
        // allowing the BTI and its ContextBank to be torn down.
        while in_flight {
            SmmuEnv::sleep_usec(100);
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            in_flight = irq_guard.context_irqs()[idx].in_flight;
        }
    }

    /// Re-enables the context bank fault interrupt for `bti`.
    pub(crate) fn reenable_context_bank_irq(&self, bti: &SmmuBti) {
        ksync::lock!(let _guard = self.lock_lock());
        ksync::lock!(let bti_guard = bti.lock_lock());
        // `bti.lock` already disabled interrupts via `IrqSavePolicy`, so acquire `irq_lock`
        // with `SmmuNoIrqSavePolicy`.
        ksync::lock!(let mut irq_guard = self.lock_irq_lock_policy::<SmmuNoIrqSavePolicy>());

        let cb_ndx = SmmuBti::cb_ndx_locked(&bti_guard);
        let idx = cb_ndx as usize;
        if idx >= irq_guard.context_irqs().len() {
            // Temporary porting note: the C++ `ShutdownContextBankIrq` and
            // `ReenableContextBankIrq` had their "re-enable" / "disable" warning messages swapped;
            // preserved for 1:1 parity here and fixed in a follow-up commit.
            dprintf!(
                INFO,
                "{} : WARNING - Failed to disable context IRQ with BTI, invalid index ({} >= {})\n",
                self.name(),
                cb_ndx,
                irq_guard.context_irqs().len()
            );
            return;
        }

        let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
        if !vec.registered || vec.enabled || vec.in_flight {
            return;
        }

        if *bti_guard.mode() != BtiMode::Bypass && *bti_guard.mode() != BtiMode::Translation {
            return;
        }

        let cb_base = self.get_cb_base(cb_ndx);
        Sctlr::read_from(&cb_base).set_cfie(1).write_to(&cb_base);
        let _ = SmmuEnv::unmask_interrupt(vec.irq_def.num);
        vec.enabled = true;
    }

    /// Adopts an already-enabled Stream Match Register Group and its associated Context Bank
    /// (if in translation mode) during early-init handoff.
    ///
    /// When adopting SMRG state, if the SMRG configuration references a context bank, one of two
    /// things may end up happening:
    ///
    /// 1) If the SMRG state references a context bank which has not yet been adopted, a new
    ///    `ContextBank` object will be created and adopted, and a new `SmmuBti` object will be
    ///    created to hold the newly adopted SMRG and `ContextBank` objects. The existing SMRG and
    ///    `ContextBank` register state will be left as is. Internal bookkeeping will be updated to
    ///    match the pre-existing state.
    /// 2) If the SMRG state references a `ContextBank` which has already been adopted by a
    ///    different adopted SMRG, the newly adopted SMRG will be added to the existing `SmmuBti`
    ///    which was created to contain the pre-existing SMRG(s)/`ContextBank` state.
    fn adopt_smrg(
        &self,
        guard: &mut Pin<&mut SmmuLockGuard<'_>>,
        smrg_ndx: u32,
        reg_stream_ids: SmrValue,
    ) -> Result<(), Status> {
        // The index we are being given must be valid, the group must be already
        // enabled, and not currently in use according to our bookkeeping. At this
        // point in initialization, we should also be able to ASSERT all of these
        // things.
        debug_assert!(smrg_ndx < self.num_smrgs());
        debug_assert!(guard.available_smrgs().test_bit(smrg_ndx));
        debug_assert_ne!(Smr::read_at(&self.gr0_base(), smrg_ndx).valid(), 0);

        // Go over our existing BTIs. None of the stream IDs we are attempting to
        // claim should currently be in use by any of our active BTIs. If they are,
        // we have a problem as it implies that there are at least two SMRGs
        // configured to match one or more of the same stream ID, which is undefined
        // behavior for SMMUv2 hardware.
        //
        // Return an error; we will disable and lock down this specific SMRG (at
        // `smrg_ndx`) after we have finished adoption of the rest of the early-init
        // SMRGs.
        for bti in guard.bti_list().iter() {
            if bti.smr_intersects(reg_stream_ids) {
                return Err(Status::ALREADY_BOUND);
            }
        }

        // Create a new SMRG, and adopt its initial state from existing register state.
        let mut smrg = StreamMatchRegGroup::create_and_adopt(
            &self.controller_params(guard),
            smrg_ndx,
            reg_stream_ids,
        )?;

        // If this SMRG is in translation mode, go looking for an existing BTI
        // which is already using the same context bank as this SMRG. If we find one,
        // just add this SMRG to it. Otherwise, we'll need to create a new BTI.
        if smrg.mode() == S2crType::Translation {
            let mut found_bti: Option<RefPtr<SmmuBti>> = None;
            for bti in guard.bti_list().iter() {
                if bti.cb_ndx() == smrg.cb_ndx() {
                    found_bti = Some(RefPtr::from_ref(bti));
                    break;
                }
            }
            if let Some(existing_bti) = found_bti {
                existing_bti.add_smrg(guard, smrg);
                return Ok(());
            }
        }

        // We didn't find an existing BTI using this context bank (or the SMRG is not in
        // translation mode). If the SMRG is in translation mode, adopt its context bank
        // as well, then create a new BTI in the Adopted state to hold these resources.
        let context_bank = if smrg.mode() == S2crType::Translation {
            match ContextBank::create_and_adopt(&self.controller_params(guard), smrg.cb_ndx()) {
                Ok(cb) => Some(cb),
                Err(err) => {
                    // Temporary porting note: the C++ `AdoptSmrg` returned here without
                    // resetting the SMRG's bookkeeping, tripping `~StreamMatchRegGroup`'s debug
                    // asserts. Disable the SMRG instead (the C++ would have locked it down
                    // moments later in `Lockdown` anyway, since it was never marked as in use).
                    //
                    // What the hardware sees differently from the C++ on this path (in
                    // practice, an allocation failure during early init):
                    //
                    // 1) The SMR is cleared (VALID, MASK and ID zero) and the S2CR set to Fault
                    //    now, not when `init` reaches `lockdown`. Until then the C++ let the
                    //    stream carry on translating through a context bank we had failed to
                    //    adopt. The order is the one `lockdown` uses (stream matches first,
                    //    context banks after), and CR0 has already been written, so the
                    //    now-unmatched stream is handled as it would be after lockdown.
                    // 2) `lockdown` still finds this SMRG available and disables it again.
                    //    Those writes repeat the values written here. As the SMR no longer
                    //    reads as valid, `lockdown` also skips its diagnostic reads (CBAR and
                    //    SCTLR of the context bank) and its log lines for this SMRG; the caller
                    //    logs the failure instead.
                    // 3) A later handoff stream ID which intersects this same SMR no longer
                    //    sees it as valid, so adoption is not attempted again. The C++ would
                    //    have called `AdoptSmrg` a second time, and if that attempt succeeded
                    //    the stream would have stayed adopted.
                    //
                    // 1) and 2) end in the state the C++ reached, sooner, and are at least as
                    // good. 3) is a real difference in outcome, but only for a configuration
                    // which lists two handoff stream IDs matching one SMR, and only when the
                    // first attempt failed and the second would not have. The C++ retry was a
                    // side effect of leaving the SMR valid, not a designed recovery, and gave a
                    // result which depended on the order of the handoff list. Here a failed
                    // adoption always ends locked down, which is the fail-closed result the
                    // C++ gave in every other case.
                    smrg.disable();
                    return Err(err);
                }
            }
        } else {
            None
        };

        let bti = SmmuBti::create(self, guard, smrg, context_bank, BtiMode::Adopted)?;
        guard.as_mut().bti_list_mut().push_back(bti);
        Ok(())
    }
}

/// Stack-allocated buffer for formatting register dump tags and fault lines in `no_std`.
pub(crate) struct TagBuf {
    buf: [u8; 160],
    len: usize,
}

impl TagBuf {
    pub(crate) const fn new() -> Self {
        Self { buf: [0u8; 160], len: 0 }
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    pub(crate) fn as_str(&self) -> &str {
        // SAFETY: `write_str` only appends valid UTF-8 bytes into `self.buf[..self.len]`.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl core::fmt::Write for TagBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.len);
        let mut to_copy = bytes.len().min(avail);
        while !s.is_char_boundary(to_copy) {
            to_copy -= 1;
        }
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

/// Parsed console argument passed to [`Smmu::console_cmd`].
#[derive(Clone, Copy, Debug)]
pub struct ConsoleArg<'a> {
    /// String token.
    pub str_val: &'a str,
    /// Unsigned integer value.
    pub u: u64,
}

static DEFAULT_SMMU_NDX: AtomicU32 = AtomicU32::new(0);

fn usage(ret: i32) -> i32 {
    dprintf!(INFO, "Usage: smmu <cmd>\n");
    dprintf!(INFO, "Valid commands are:\n");
    dprintf!(INFO, "  help     : show this message\n");
    dprintf!(
        INFO,
        "  list     : list all discovered IOMMU instances and their currently managed BTIs.\n"
    );
    dprintf!(INFO, "  set <id> : set the default SMMU to use with other commands.\n");
    dprintf!(INFO, "  show <tgt> [-v] [-pmts]\n");
    dprintf!(INFO, "    Print the current high level status of the selected SMMU.\n");
    dprintf!(INFO, "    Use <tgt> to select a specific BTI to dump details about, or\n");
    dprintf!(INFO, "    \"all\" to show the status for the entire smmu\n");
    dprintf!(INFO, "  dumpregs\n");
    dprintf!(INFO, "    Print raw register information for the selected SMMU\n");
    dprintf!(INFO, "  lock <tgt>\n");
    dprintf!(INFO, "    Attempt to set the operating mode of the target BTI(s) to FAULT.\n");
    dprintf!(INFO, "  isids <tgt>\n");
    dprintf!(INFO, "    Attempt to invalidate all Stream IDs of the target BTI(s).\n");
    dprintf!(INFO, "\n");
    dprintf!(INFO, "Params:\n");
    dprintf!(INFO, "  <tgt>   : One of (all | bti <N> | sid <Stream ID>)\n");
    dprintf!(
        INFO,
        "  <--id>  : The ID of the SMMU to operate on, instead of the current default.\n"
    );
    dprintf!(
        INFO,
        "  [-v]    : Optionally enable verbose output, for sub-commands who support it.\n"
    );
    dprintf!(INFO, "  [-pmts] : Show detailed PMT status when using the show command\n");
    ret
}

fn find_optional_flag(args: &[ConsoleArg<'_>], option: &str) -> bool {
    args.iter().any(|arg| arg.str_val == option)
}

fn find_optional_u32(args: &[ConsoleArg<'_>], option: &str) -> Result<Option<u32>, Status> {
    let mut ret: Option<u32> = None;
    let mut i = 0usize;
    while i < args.len() {
        if args[i].str_val == option {
            if ret.is_some() {
                dprintf!(INFO, "Multiple \"{}\" options specified!\n", option);
                return Err(Status::ALREADY_EXISTS);
            }
            i += 1;
            if i >= args.len() {
                dprintf!(INFO, "No value specified for option \"{}\"!\n", option);
                return Err(Status::INVALID_ARGS);
            }
            let val = args[i].u;
            if val > (u32::MAX as u64) {
                dprintf!(INFO, "Value out of range for for option \"{}\" ({})!\n", option, val);
                return Err(Status::OUT_OF_RANGE);
            }
            ret = Some(val as u32);
        }
        i += 1;
    }
    Ok(ret)
}

/// Formats and prints a register and its bitfields to the kernel console.
pub(crate) fn dump_reg(
    tag: &str,
    raw_val: u64,
    byte_width: usize,
    reg_offset: u32,
    fields: &[FieldDesc],
    base: &SmmuMmio,
    ilvl: usize,
) {
    let indent = ilvl * 2;
    let paddr = SmmuEnv::periph_vaddr_to_paddr(base.base()).wrapping_add(reg_offset as u64);
    dprintf!(INFO, "{:indent$}{} : [{:#x}]\n", "", tag, paddr, indent = indent);

    let mut max_name_width = "raw".len();
    for f in fields {
        if !f.name.is_empty() && !f.name.starts_with("__res") {
            max_name_width = max_name_width.max(f.name.len());
        }
    }

    let msb = ((byte_width * 8) - 1) as u32;
    if byte_width == 8 {
        dprintf!(
            INFO,
            "{:indent$}[{:2}:{:2}] : {:>width$} : {:#018x} ({})\n",
            "",
            msb,
            0,
            "raw",
            raw_val,
            raw_val,
            indent = indent,
            width = max_name_width
        );
    } else {
        dprintf!(
            INFO,
            "{:indent$}[{:2}:{:2}] : {:>width$} : {:#010x} ({})\n",
            "",
            msb,
            0,
            "raw",
            raw_val as u32,
            raw_val as u32,
            indent = indent,
            width = max_name_width
        );
    }

    for f in fields {
        if !f.name.is_empty() && !f.name.starts_with("__res") {
            let val = f.extract(raw_val);
            if byte_width == 8 {
                dprintf!(
                    INFO,
                    "{:indent$}[{:2}:{:2}] : {:>width$} : {:#018x} ({})\n",
                    "",
                    f.msb,
                    f.lsb,
                    f.name,
                    val,
                    val,
                    indent = indent,
                    width = max_name_width
                );
            } else {
                dprintf!(
                    INFO,
                    "{:indent$}[{:2}:{:2}] : {:>width$} : {:#010x} ({})\n",
                    "",
                    f.msb,
                    f.lsb,
                    f.name,
                    val as u32,
                    val as u32,
                    indent = indent,
                    width = max_name_width
                );
            }
        }
    }
    dprintf!(INFO, "\n");
}

impl Smmu {
    /// Dumps raw register information for this SMMU (`smmu dumpregs`).
    pub fn cmd_dump_regs(&self) {
        ksync::lock!(let guard = self.lock_lock());

        let gr0_base = self.gr0_base();
        let gr1_base = self.gr1_base();
        let st = self.init_state();

        dprintf!(INFO, "Dumping configuration for {}\n", self.name());
        dprintf!(INFO, "Version    : {}.{}\n", st.idr7.major(), st.idr7.minor());
        dprintf!(INFO, "User-Bound : {}\n", if *guard.user_mode_bound() { "Yes" } else { "No" });

        let idr0 = Idr0::read_from(&gr0_base);
        dump_reg("IDR0", idr0.reg_value() as u64, 4, Idr0::OFFSET, Idr0::FIELDS, &gr0_base, 0);
        dump_reg(
            "IDR1",
            Idr1::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr1::OFFSET,
            Idr1::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "IDR2",
            Idr2::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr2::OFFSET,
            Idr2::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "IDR7",
            Idr7::read_from(&gr0_base).reg_value() as u64,
            4,
            Idr7::OFFSET,
            Idr7::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "CR0",
            Cr0::read_from(&gr0_base).reg_value() as u64,
            4,
            Cr0::OFFSET,
            Cr0::FIELDS,
            &gr0_base,
            0,
        );
        dump_reg(
            "CR2",
            Cr2::read_from(&gr0_base).reg_value() as u64,
            4,
            Cr2::OFFSET,
            Cr2::FIELDS,
            &gr0_base,
            0,
        );

        let mut tag = TagBuf::new();
        macro_rules! fmt_tag {
            ($($arg:tt)*) => {{
                tag.clear();
                let _ = core::write!(&mut tag, $($arg)*);
                tag.as_str()
            }};
        }

        // If this unit supports stream matching, dump the state of the available
        // stream matching register groups. If a stream matching group is in
        // translation mode, and has a valid context bank ID configured, make a note
        // of it so we can come back later and dump the status of the in-use context
        // banks.
        let mut in_use_cbs = Bitmask128::new();
        if idr0.sms() != 0 {
            for i in 0..idr0.numsmrg() {
                let smr = Smr::read_at(&gr0_base, i);
                if smr.valid() != 0 {
                    dump_reg(
                        fmt_tag!("SMR{}", i),
                        smr.reg_value() as u64,
                        4,
                        Smr::offset(i),
                        Smr::FIELDS,
                        &gr0_base,
                        0,
                    );
                    // The format of S2CR registers depends on the type value currently programmed
                    // into the register.
                    let s2cr = S2cr::read_at(&gr0_base, i);
                    let s2cr_tag =
                        fmt_tag!("S2CR{} ({})", i, arm_s2cr_type_to_string(s2cr.s2cr_type()));
                    match s2cr.s2cr_type() {
                        S2crType::Translation => {
                            let s2cr_trans = S2crTranslation::from_value(s2cr.reg_value());
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crTranslation::FIELDS,
                                &gr0_base,
                                0,
                            );
                            if s2cr_trans.cbndx() >= self.num_cbs() {
                                dprintf!(INFO, "Invalid Context Bank ID {}\n", s2cr_trans.cbndx());
                            } else {
                                in_use_cbs.set_bit(s2cr_trans.cbndx());
                            }
                        }
                        S2crType::Bypass => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crBypass::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                        S2crType::Fault => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2crFault::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                        S2crType::Invalid => {
                            dump_reg(
                                s2cr_tag,
                                s2cr.reg_value() as u64,
                                4,
                                S2cr::offset(i),
                                S2cr::FIELDS,
                                &gr0_base,
                                0,
                            );
                        }
                    }
                }
            }
        }

        // Now dump details about any in-use context banks.
        for i in 0..self.num_cbs() {
            if !in_use_cbs.test_bit(i) {
                continue;
            }

            dprintf!(INFO, "Context Bank {}:\n", i);
            let cbar = Cbar::read_at(&gr1_base, i);
            let cbar_fields = match cbar.cbar_type() {
                CbarType::S2Translation => CbarS2Translation::FIELDS,
                CbarType::S1TS2Bypass => CbarS1TS2Bypass::FIELDS,
                CbarType::S1TS2Fault => CbarS1TS2Fault::FIELDS,
                CbarType::S1TS2Translate => CbarS1TS2Translate::FIELDS,
            };
            dump_reg(
                fmt_tag!("CBAR{} ({})", i, arm_cbar_type_to_string(cbar.cbar_type())),
                cbar.reg_value() as u64,
                4,
                Cbar::offset(i),
                cbar_fields,
                &gr1_base,
                0,
            );

            let cba2r = Cba2r::read_at(&gr1_base, i);
            dump_reg(
                fmt_tag!("CBA2R{}", i),
                cba2r.reg_value() as u64,
                4,
                Cba2r::offset(i),
                Cba2r::FIELDS,
                &gr1_base,
                0,
            );

            let cbfrsynra = Cbfrsynra::read_at(&gr1_base, i);
            dump_reg(
                fmt_tag!("CBFRSYNRA{}", i),
                cbfrsynra.reg_value() as u64,
                4,
                Cbfrsynra::offset(i),
                Cbfrsynra::FIELDS,
                &gr1_base,
                0,
            );

            let cb_base = self.get_cb_base(i);
            if cbar.cbar_type() != CbarType::S2Translation {
                dump_reg(
                    fmt_tag!("SCTLR({})", i),
                    Sctlr::read_from(&cb_base).reg_value() as u64,
                    4,
                    Sctlr::OFFSET,
                    Sctlr::FIELDS,
                    &cb_base,
                    0,
                );
                let tcr = Tcr::read_from(&cb_base);
                let addr_mode = if cba2r.va64() == Cba2rVa64::Va64 {
                    AddrMode::Mode64Bit
                } else if tcr.eae() != 0 {
                    AddrMode::Ext32Bit
                } else {
                    AddrMode::Mode32Bit
                };
                match addr_mode {
                    AddrMode::Mode32Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 0, EAE == 0)", i),
                            tcr.reg_value() as u64,
                            4,
                            Tcr32Bit::OFFSET,
                            Tcr32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr0_32Bit::OFFSET,
                            Ttbr0_32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr1_32Bit::OFFSET,
                            Ttbr1_32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Ext32Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 0, EAE == 1)", i),
                            tcr.reg_value() as u64,
                            4,
                            TcrExt32Bit::OFFSET,
                            TcrExt32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0Ext32Bit::OFFSET,
                            Ttbr0Ext32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1Ext32Bit::OFFSET,
                            Ttbr1Ext32Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Mode64Bit => {
                        dump_reg(
                            fmt_tag!("TCR({}) (CBA2R.VA64 == 1)", i),
                            tcr.reg_value() as u64,
                            4,
                            Tcr64Bit::OFFSET,
                            Tcr64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR0({})", i),
                            Ttbr0_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0_64Bit::OFFSET,
                            Ttbr0_64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                        dump_reg(
                            fmt_tag!("TTBR1({})", i),
                            Ttbr1_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1_64Bit::OFFSET,
                            Ttbr1_64Bit::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                    AddrMode::Invalid => {
                        dump_reg(
                            fmt_tag!("TCR({}) (Bad AddrMode {})", i, addr_mode as u32),
                            tcr.reg_value() as u64,
                            4,
                            Tcr::OFFSET,
                            Tcr::FIELDS,
                            &cb_base,
                            0,
                        );
                    }
                }
            }
        }
    }

    /// Prints the high-level status of this SMMU and its BTIs (`smmu show all`).
    pub fn cmd_show_all(&self, show_pmts: bool, verbose: bool) {
        ksync::lock!(let guard = self.lock_lock());
        self.cmd_show_all_locked(&guard, show_pmts, verbose);
    }

    fn cmd_show_all_locked(&self, guard: &SmmuLockGuard<'_>, show_pmts: bool, verbose: bool) {
        let st = self.init_state();

        dprintf!(INFO, "Dumping configuration for {}\n", self.name());
        dprintf!(INFO, "Version    : {}.{}\n", st.idr7.major(), st.idr7.minor());
        dprintf!(INFO, "User-Bound : {}\n", if *guard.user_mode_bound() { "Yes" } else { "No" });
        dprintf!(INFO, "OpMode     : {}\n", arm_smmu_mode_to_string(st.op_mode));
        dprintf!(INFO, "BTI Count  : {}\n", guard.bti_list().len());

        for (i, bti) in guard.bti_list().iter().enumerate() {
            let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
            bti.get_name(&mut bti_name_buf);
            let bti_name = SmmuBti::name_display(&bti_name_buf);
            dprintf!(INFO, "\nBTI #{} (\"{}\")\n", i, bti_name);
            bti.cmd_show(1, show_pmts, verbose);
        }
    }

    fn find_cmd_target_locked(
        &self,
        guard: &SmmuLockGuard<'_>,
        args: &[ConsoleArg<'_>],
        out_ndx: Option<&mut u32>,
    ) -> Result<Option<RefPtr<SmmuBti>>, Status> {
        let mut local_ndx = 0u32;
        let ndx_ref = match out_ndx {
            Some(r) => {
                *r = 0;
                r
            }
            None => &mut local_ndx,
        };

        let Some(first) = args.first() else {
            dprintf!(INFO, "No target specified.  One of (all|bti <N>|sid <Id>) is required.\n");
            return Err(Status::INVALID_ARGS);
        };

        let tgt_type = first.str_val;
        if tgt_type == "all" {
            return Ok(None);
        }

        let is_bti = tgt_type == "bti";
        if !is_bti && tgt_type != "sid" {
            dprintf!(INFO, "Invalid target \"{}\"\n", tgt_type);
            return Err(Status::INVALID_ARGS);
        }

        if args.len() < 2 {
            dprintf!(INFO, "Insufficient arguments for \"{}\" target\n", tgt_type);
            return Err(Status::INVALID_ARGS);
        }

        let tgt_val = args[1].u.min(u32::MAX as u64) as u32;
        if !is_bti && (tgt_val & !self.valid_stream_id_mask()) != 0 {
            dprintf!(INFO, "Invalid SID {:#x}\n", tgt_val);
            return Err(Status::INVALID_ARGS);
        }

        for bti in guard.bti_list().iter() {
            if is_bti {
                if *ndx_ref == tgt_val {
                    return Ok(Some(RefPtr::from_ref(bti)));
                }
            } else if bti.smr_intersects(SmrValue::from_raw(tgt_val, self.valid_stream_id_mask())) {
                return Ok(Some(RefPtr::from_ref(bti)));
            }
            *ndx_ref += 1;
        }

        dprintf!(INFO, "Failed to find {} target {:#x} in {}\n", tgt_type, tgt_val, self.name());
        Err(Status::NOT_FOUND)
    }

    fn cmd_show(&self, args: &[ConsoleArg<'_>], cmd_ndx: usize) -> i32 {
        ksync::lock!(let guard = self.lock_lock());

        let arg_ndx = cmd_ndx + 1;
        debug_assert!(arg_ndx <= args.len());

        let tgt = match self.find_cmd_target_locked(&guard, &args[arg_ndx..], None) {
            Ok(t) => t,
            Err(status) => return status.into_raw(),
        };

        let verbose = find_optional_flag(args, "-v");
        let show_pmts = find_optional_flag(args, "-pmts");
        if let Some(tgt) = tgt {
            tgt.cmd_show(0, show_pmts, verbose);
        } else {
            self.cmd_show_all_locked(&guard, show_pmts, verbose);
        }

        0
    }

    fn cmd_bti_op<F>(&self, args: &[ConsoleArg<'_>], cmd_ndx: usize, mut op: F) -> i32
    where
        F: FnMut(&Smmu, &SmmuBti, u32),
    {
        ksync::lock!(let guard = self.lock_lock());

        debug_assert!(cmd_ndx < args.len());
        let tgt_ndx = cmd_ndx + 1;

        if tgt_ndx >= args.len() {
            dprintf!(INFO, "Insufficient arguments for \"{}\" command\n", args[cmd_ndx].str_val);
            return usage(Status::INVALID_ARGS.into_raw());
        }

        let mut found_ndx = 0u32;
        let bti_tgt =
            match self.find_cmd_target_locked(&guard, &args[tgt_ndx..], Some(&mut found_ndx)) {
                Ok(t) => t,
                Err(status) => return status.into_raw(),
            };

        if let Some(bti) = bti_tgt {
            op(self, &bti, found_ndx);
        } else {
            for (ndx, bti) in guard.bti_list().iter().enumerate() {
                op(self, bti, ndx as u32);
            }
        }

        0
    }

    /// Dispatches the `smmu` kernel console command.
    pub fn console_cmd(args: &[ConsoleArg<'_>], _flags: u32) -> i32 {
        // We need at least two arguments to have a valid command.
        if args.len() < 2 {
            dprintf!(INFO, "Bad argument count ({}).\n\n", args.len());
            return usage(Status::INVALID_ARGS.into_raw());
        }

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Cmd {
            Invalid,
            Help,
            List,
            SetDefault,
            Show,
            DumpRegs,
            Lock,
            InvalidateSids,
        }

        const CMD_MAP: &[(&str, Cmd)] = &[
            ("help", Cmd::Help),
            ("list", Cmd::List),
            ("set", Cmd::SetDefault),
            ("show", Cmd::Show),
            ("dumpregs", Cmd::DumpRegs),
            ("lock", Cmd::Lock),
            ("isids", Cmd::InvalidateSids),
        ];

        // Find command in the argument list, and remember its index so that the
        // command handler can parse its positional arguments. The command is the
        // first entry in the argument list which does not start with a "-".
        let mut cmd_ndx = 0usize;
        let mut cmd = Cmd::Invalid;
        for (i, arg) in args.iter().enumerate().skip(1) {
            if !arg.str_val.starts_with('-') {
                cmd_ndx = i;
                for &(name, val) in CMD_MAP {
                    if arg.str_val == name {
                        cmd = val;
                    }
                }
                if cmd == Cmd::Invalid {
                    dprintf!(INFO, "Invalid command \"{}\"\n", arg.str_val);
                    return usage(Status::INVALID_ARGS.into_raw());
                }
                break;
            }
        }

        if cmd_ndx == 0 {
            dprintf!(INFO, "Missing command!\n");
            return usage(Status::INVALID_ARGS.into_raw());
        }

        // If the command is anything but "help" or "list", then we are going to need
        // to find an SMMU to operate on.
        let mut chosen_smmu: Option<RefPtr<Smmu>> = None;
        if cmd != Cmd::Help && cmd != Cmd::List {
            let maybe_id = match find_optional_u32(args, "--id") {
                Ok(id) => id,
                Err(status) => return usage(status.into_raw()),
            };

            // If we are attempting to set a new default, then use a value explicitly
            // passed via the `--id` option, if provided, or expect a positional
            // argument otherwise.
            let tgt_ndx = if cmd == Cmd::SetDefault {
                if let Some(id) = maybe_id {
                    id
                } else {
                    let id_ndx = cmd_ndx + 1;
                    if id_ndx >= args.len() {
                        dprintf!(INFO, "No default SMMU ID specified.\n");
                        return usage(Status::INVALID_ARGS.into_raw());
                    }
                    args[id_ndx].u as u32
                }
            } else {
                // `Relaxed` suffices: `DEFAULT_SMMU_NDX` is an advisory index hint for interactive
                // console commands and does not synchronize any other state (`SmmuInstanceLock` guards
                // `INSTANCES` lookup below).
                maybe_id.unwrap_or_else(|| DEFAULT_SMMU_NDX.load(Ordering::Relaxed))
            };

            // Lock and try to find the target SMMU. Error out if we cannot.
            {
                ksync::lock!(let mut instance_guard = SmmuInstanceLock::lock());
                let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
                for (ndx, smmu) in list.iter().enumerate() {
                    if (ndx as u32) == tgt_ndx {
                        chosen_smmu = Some(RefPtr::from_ref(smmu));
                    }
                }
            }

            let Some(ref smmu) = chosen_smmu else {
                dprintf!(INFO, "Failed to find SMMU #{}\n", tgt_ndx);
                return usage(Status::INVALID_ARGS.into_raw());
            };

            // We found our SMMU. If we are setting the default, update the global
            // default value and get out. Otherwise, continue processing with our
            // chosen SMMU.
            if cmd == Cmd::SetDefault {
                dprintf!(INFO, "Default SMMU is now \"{}\".\n", smmu.name());
                // `Relaxed` suffices: `DEFAULT_SMMU_NDX` is a standalone console index hint and
                // does not publish or synchronize any other memory.
                DEFAULT_SMMU_NDX.store(tgt_ndx, Ordering::Relaxed);
                return 0;
            }
        }

        // Dispatch the command.
        match cmd {
            Cmd::Help => usage(0),
            Cmd::List => {
                ksync::lock!(let mut instance_guard = SmmuInstanceLock::lock());
                let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
                dprintf!(INFO, "Discovered {} SMMU instances\n", list.len());
                for (id, smmu) in list.iter().enumerate() {
                    ksync::lock!(let guard = smmu.lock_lock());
                    dprintf!(
                        INFO,
                        "[{}] - {} (mode {}, user-bound : {})\n",
                        id,
                        smmu.name(),
                        arm_smmu_mode_to_string(smmu.op_mode()),
                        if *guard.user_mode_bound() { "Yes" } else { "No" }
                    );

                    for (bti_ndx, bti) in guard.bti_list().iter().enumerate() {
                        let mut sid_buf = [0u8; 128];
                        let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
                        bti.get_name(&mut bti_name_buf);
                        let bti_name = SmmuBti::name_display(&bti_name_buf);

                        ksync::lock!(let bti_pmt_guard = bti.lock_pmt_lock());
                        ksync::lock!(let bti_guard = bti.lock_lock());
                        let sid_str = SmmuBti::render_sid_list_locked(&bti_guard, &mut sid_buf);
                        dprintf!(
                            INFO,
                            "    BTI[{}] - Mode {}{} StreamIDs [{}] Name \"{}\"\n",
                            bti_ndx,
                            bti_mode_to_string(*bti_guard.mode()),
                            if *bti_pmt_guard.orphaned() { " (orphaned)" } else { "" },
                            sid_str,
                            bti_name
                        );
                    }
                }
                0
            }
            Cmd::SetDefault => {
                // Set default should have been handled earlier.
                usage(Status::INVALID_ARGS.into_raw())
            }
            Cmd::Show => {
                // "show" takes an optional BTI ID or target selector.
                let smmu = chosen_smmu.expect("chosen_smmu must be set for Show");
                smmu.cmd_show(args, cmd_ndx)
            }
            Cmd::DumpRegs => {
                // "dumpregs" takes no extra arguments.
                let smmu = chosen_smmu.expect("chosen_smmu must be set for DumpRegs");
                smmu.cmd_dump_regs();
                0
            }
            Cmd::Lock => {
                let smmu = chosen_smmu.expect("chosen_smmu must be set for Lock");
                smmu.cmd_bti_op(args, cmd_ndx, |_smmu, bti, ndx| {
                    bti.cmd_lock(ndx);
                })
            }
            Cmd::InvalidateSids => {
                let smmu = chosen_smmu.expect("chosen_smmu must be set for InvalidateSids");
                smmu.cmd_bti_op(args, cmd_ndx, |smmu, bti, ndx| {
                    let mut bti_name_buf = [0u8; ZX_MAX_NAME_LEN];
                    bti.get_name(&mut bti_name_buf);
                    let bti_name = SmmuBti::name_display(&bti_name_buf);
                    match bti.invalidate_sids() {
                        Ok(()) => {
                            dprintf!(
                                INFO,
                                "{}: Stream IDs for BTI index #{} (\"{}\") have been \
                                 invalidated.\n",
                                smmu.name(),
                                ndx,
                                bti_name
                            );
                        }
                        Err(status) => {
                            dprintf!(
                                INFO,
                                "{}: Failed to invalidate Stream IDs for BTI index #{} (\"{}\") \
                                 (err {}).\n",
                                smmu.name(),
                                ndx,
                                bti_name,
                                status.into_raw()
                            );
                        }
                    }
                })
            }
            Cmd::Invalid => {
                // It should be impossible to have anything but a valid command at this
                // point.
                unreachable!()
            }
        }
    }
}
