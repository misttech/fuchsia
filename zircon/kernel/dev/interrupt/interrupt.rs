// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// The maximum number of interrupts supported in the system.
pub const MAX_INTERRUPTS: usize = 1024;

/// The maximum number of MSI IRQs.
pub const MAX_MSI_IRQS: usize = 32;

/// Represents an interrupt vector number.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct InterruptVector(pub u32);

impl InterruptVector {
    /// Creates a new `InterruptVector` from a raw `u32`.
    #[inline]
    pub const fn from_raw(vector: u32) -> Self {
        Self(vector)
    }

    /// Converts this `InterruptVector` to its underlying `u32` value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl From<u32> for InterruptVector {
    #[inline]
    fn from(vector: u32) -> Self {
        Self(vector)
    }
}

impl From<InterruptVector> for u32 {
    #[inline]
    fn from(vector: InterruptVector) -> u32 {
        vector.0
    }
}

impl From<InterruptVector> for usize {
    #[inline]
    fn from(vector: InterruptVector) -> usize {
        vector.0 as usize
    }
}

impl core::ops::Deref for InterruptVector {
    type Target = u32;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// An interrupt handler callback and its opaque context pointer.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InterruptHandler {
    /// Opaque context pointer passed to `fn`.
    pub cookie: *mut core::ffi::c_void,
    /// Callback function invoked when the interrupt fires.
    pub r#fn: Option<extern "C" fn(*mut core::ffi::c_void) -> ()>,
}

zr::static_assert_size_and_align!(InterruptHandler, 16, 8);

impl InterruptHandler {
    /// An empty `InterruptHandler` used to unregister an interrupt handler.
    pub const DEFAULT: Self = Self { cookie: core::ptr::null_mut(), r#fn: None };

    /// Returns `true` if a callback function is present.
    pub fn present(&self) -> bool {
        self.r#fn.is_some()
    }

    /// Invokes the interrupt handler callback with `self.cookie`.
    pub fn invoke(&self) {
        debug_assert!(self.present());
        (self.r#fn.unwrap())(self.cookie)
    }
}

// SAFETY: `InterruptHandler` represents an opaque C-ABI callback
// `(cookie, fn)`. Callers registering a handler guarantee that the callback
// and its cookie context are safe to transfer and invoke from interrupt
// context across any CPU. Synchronization of the storage slot itself is
// managed by the enclosing interrupt manager.
unsafe impl Send for InterruptHandler {}
unsafe impl Sync for InterruptHandler {}

/// The trigger mode of an interrupt.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptTriggerMode {
    /// Edge triggered interrupt.
    Edge = 0,
    /// Level triggered interrupt.
    Level = 1,
}

impl InterruptTriggerMode {
    /// Returns a string representation of the trigger mode.
    pub fn string(self) -> &'static str {
        match self {
            InterruptTriggerMode::Edge => "edge",
            InterruptTriggerMode::Level => "level",
        }
    }
}

/// The polarity of an interrupt.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptPolarity {
    /// Active high or rising edge polarity.
    High = 0,
    /// Active low or falling edge polarity.
    Low = 1,
}

impl InterruptPolarity {
    /// Returns a string representation of the polarity.
    pub fn string(self) -> &'static str {
        match self {
            InterruptPolarity::High => "high",
            InterruptPolarity::Low => "low",
        }
    }
}

/// A structure which holds the state of a block of IRQs allocated by the
/// platform to be used for delivering MSI or MSI-X interrupts.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MsiBlock {
    /// The target write transaction physical address.
    pub tgt_addr: u64,
    /// The data which the device should write when triggering an IRQ. Note,
    /// only the lower 16 bits are used when the block has been allocated for MSI
    /// instead of MSI-X.
    pub tgt_data: u32,
    /// The first IRQ id in the allocated block.
    pub base_irq_id: u32,
    /// The number of irqs in the allocated block.
    pub num_irq: u32,
    /// Whether or not this block has been allocated.
    pub allocated: bool,
    /// 32 bit if true, 64 bit otherwise.
    pub is_32bit: bool,
}

zr::static_assert_size_and_align!(InterruptVector, 4, 4);
zr::static_assert_size_and_align!(InterruptTriggerMode, 4, 4);
zr::static_assert_size_and_align!(InterruptPolarity, 4, 4);
zr::static_assert_size_and_align!(MsiBlock, 24, 8);

mod sys {
    use super::{InterruptHandler, InterruptPolarity, InterruptTriggerMode, MsiBlock};

    unsafe extern "C" {
        pub fn mask_interrupt(vector: u32) -> zx_types::zx_status_t;
        pub fn unmask_interrupt(vector: u32) -> zx_types::zx_status_t;
        #[cfg(target_arch = "aarch64")]
        pub fn deactivate_interrupt(vector: u32) -> zx_types::zx_status_t;
        pub fn configure_interrupt(
            vector: u32,
            tm: InterruptTriggerMode,
            pol: InterruptPolarity,
        ) -> zx_types::zx_status_t;
        pub fn get_interrupt_config(
            vector: u32,
            tm: *mut InterruptTriggerMode,
            pol: *mut InterruptPolarity,
        ) -> zx_types::zx_status_t;
        pub fn register_int_handler(
            vector: u32,
            handler: InterruptHandler,
        ) -> zx_types::zx_status_t;
        pub fn remap_interrupt(vector: u32) -> u32;
        pub fn is_valid_interrupt(vector: u32, flags: u32) -> bool;
        pub fn msi_is_supported() -> bool;
        pub fn msi_alloc_block(
            requested_irqs: u32,
            can_target_64bit: bool,
            is_msix: bool,
            out_block: *mut MsiBlock,
        ) -> zx_types::zx_status_t;
        pub fn msi_free_block(block: *mut MsiBlock);
        pub fn msi_supports_masking() -> bool;
        pub fn msi_mask_unmask(block: *const MsiBlock, msi_id: u32, mask: bool);
        pub fn msi_register_handler(block: *const MsiBlock, msi_id: u32, handler: InterruptHandler);
    }
}

pub use sys::msi_register_handler;

/// Masks the specified interrupt `vector`.
#[inline]
pub fn mask_interrupt(vector: u32) -> Result<(), zx_status::Status> {
    // SAFETY: `sys::mask_interrupt` has no memory safety preconditions.
    zx_status::Status::ok(unsafe { sys::mask_interrupt(vector) })
}

/// Unmasks the specified interrupt `vector`.
#[inline]
pub fn unmask_interrupt(vector: u32) -> Result<(), zx_status::Status> {
    // SAFETY: `sys::unmask_interrupt` has no memory safety preconditions.
    zx_status::Status::ok(unsafe { sys::unmask_interrupt(vector) })
}

/// Deactivates the specified interrupt `vector` on AArch64.
#[cfg(target_arch = "aarch64")]
#[inline]
pub fn deactivate_interrupt(vector: u32) -> Result<(), zx_status::Status> {
    // SAFETY: `sys::deactivate_interrupt` has no memory safety preconditions.
    zx_status::Status::ok(unsafe { sys::deactivate_interrupt(vector) })
}

/// Configures the trigger mode and polarity for `vector`.
#[inline]
pub fn configure_interrupt(
    vector: u32,
    tm: InterruptTriggerMode,
    pol: InterruptPolarity,
) -> Result<(), zx_status::Status> {
    // SAFETY: `sys::configure_interrupt` has no memory safety preconditions.
    zx_status::Status::ok(unsafe { sys::configure_interrupt(vector, tm, pol) })
}

/// Fetches the current trigger mode and polarity configuration for `vector`.
#[inline]
pub fn get_interrupt_config(
    vector: u32,
    tm: &mut InterruptTriggerMode,
    pol: &mut InterruptPolarity,
) -> Result<(), zx_status::Status> {
    // SAFETY: `tm` and `pol` are valid mutable references.
    zx_status::Status::ok(unsafe { sys::get_interrupt_config(vector, tm, pol) })
}

/// Registers or unregisters an interrupt `handler` for `vector`.
///
/// # Safety
///
/// If `handler` is present, its callback and `cookie` must remain valid until the handler is
/// unregistered by passing [`InterruptHandler::DEFAULT`].
#[inline]
pub unsafe fn register_int_handler(
    vector: u32,
    handler: InterruptHandler,
) -> Result<(), zx_status::Status> {
    // SAFETY: Guaranteed by the caller.
    zx_status::Status::ok(unsafe { sys::register_int_handler(vector, handler) })
}

/// Remaps an interrupt `vector` number to the platform's internal IRQ number.
#[inline]
pub fn remap_interrupt(vector: u32) -> u32 {
    // SAFETY: `sys::remap_interrupt` has no memory safety preconditions.
    unsafe { sys::remap_interrupt(vector) }
}

/// Returns `true` if `vector` is a valid interrupt vector for `flags`.
#[inline]
pub fn is_valid_interrupt(vector: u32, flags: u32) -> bool {
    // SAFETY: `sys::is_valid_interrupt` has no memory safety preconditions.
    unsafe { sys::is_valid_interrupt(vector, flags) }
}

/// Returns `true` if the platform interrupt controller supports MSI.
#[inline]
pub fn msi_is_supported() -> bool {
    // SAFETY: `sys::msi_is_supported` has no memory safety preconditions.
    unsafe { sys::msi_is_supported() }
}

/// Allocates a block of `requested_irqs` MSI interrupts into `out_block`.
#[inline]
pub fn msi_alloc_block(
    requested_irqs: u32,
    can_target_64bit: bool,
    is_msix: bool,
    out_block: &mut MsiBlock,
) -> Result<(), zx_status::Status> {
    // SAFETY: `out_block` is a valid mutable reference to an `MsiBlock`.
    zx_status::Status::ok(unsafe {
        sys::msi_alloc_block(requested_irqs, can_target_64bit, is_msix, out_block)
    })
}

/// Frees a previously allocated MSI `block`.
#[inline]
pub fn msi_free_block(block: &mut MsiBlock) {
    // SAFETY: `block` is a valid mutable reference to an `MsiBlock`.
    unsafe { sys::msi_free_block(block) }
}

/// Returns `true` if the platform interrupt controller supports MSI masking.
#[inline]
pub fn msi_supports_masking() -> bool {
    // SAFETY: `sys::msi_supports_masking` has no memory safety preconditions.
    unsafe { sys::msi_supports_masking() }
}

/// Masks or unmasks `msi_id` within `block` at the platform interrupt controller.
#[inline]
pub fn msi_mask_unmask(block: &MsiBlock, msi_id: u32, mask: bool) {
    // SAFETY: `block` is a valid reference to an `MsiBlock`.
    unsafe { sys::msi_mask_unmask(block, msi_id, mask) }
}
