// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel stacks: the machine stack of a kernel thread and, when enabled, its unsafe stack and
//! shadow call stack.

use super::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use super::pmm;
use super::vm_address_region::{VmAddressRegion, flag};
use super::vm_aspace::VmAspace;
use super::vm_object_paged::VmObjectPaged;
use crate::counters::define_kcounter;
use crate::thread_stack::internal::STACK_GUARD_REGION_SIZE;
use boot_options::BootOptions;
use core::ffi::{c_int, c_void};
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr;
use debug::{dprintf, ltracef};
use fbl::RefPtr;
use handoff::ZirconAbiSpec_Stack;
use kprint::kprint;
use zx_status::Status;
use zx_types::zx_status_t;

#[cfg(all(sanitize = "safestack", not(target_arch = "x86_64")))]
compile_error!("SafeStack in kernel supported only for x86; use ShadowCallStack");

const LOCAL_TRACE: u32 = 0;

/// The direction in which a [`Stack`] grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Growth {
    Up,
    Down,
}

/// A [`ZirconAbiSpec_Stack`] together with the name used for its mappings and its direction of
/// growth.
struct Stack {
    spec: ZirconAbiSpec_Stack,
    name: &'static str,
    growth: Growth,
}

impl Stack {
    const fn new(name: &'static str, stack: ZirconAbiSpec_Stack, growth: Growth) -> Self {
        Self { spec: stack, name, growth }
    }
}

impl Deref for Stack {
    type Target = ZirconAbiSpec_Stack;

    fn deref(&self) -> &Self::Target {
        &self.spec
    }
}

define_kcounter!(VM_KERNEL_STACK_BYTES, "vm.kstack.allocated_bytes", Sum);

const SAFE: Stack =
    Stack::new("kernel-safe-stack", crate::thread_stack::MACHINE_STACK, Growth::Down);
#[cfg(sanitize = "safestack")]
const UNSAFE: Stack =
    Stack::new("kernel-unsafe-stack", crate::thread_stack::UNSAFE_STACK, Growth::Down);
#[cfg(sanitize = "shadow-call-stack")]
const SHADOW_CALL: Stack =
    Stack::new("kernel-shadow-call-stack", crate::thread_stack::SHADOW_CALL_STACK, Growth::Up);

// `stack_canary_offset` is at most `size_bytes / 100 * 100`, since the percentage is clamped to
// 100. These keep that below `size_bytes`, so the canary is in bounds for any percentage.
zr::static_assert!((SAFE.spec.size_bytes as usize / 100) * 100 < SAFE.spec.size_bytes as usize);
#[cfg(sanitize = "safestack")]
zr::static_assert!((UNSAFE.spec.size_bytes as usize / 100) * 100 < UNSAFE.spec.size_bytes as usize);
#[cfg(sanitize = "shadow-call-stack")]
zr::static_assert!(
    (SHADOW_CALL.spec.size_bytes as usize / 100) * 100 < SHADOW_CALL.spec.size_bytes as usize
);

// `Mapping::base` and `Mapping::size` assume each stack's lower and upper guard regions are
// `STACK_GUARD_REGION_SIZE` bytes, and `unmap` relies on `Mapping::base` matching the mapped range.
zr::static_assert!(SAFE.spec.lower_guard_size_bytes == STACK_GUARD_REGION_SIZE);
zr::static_assert!(SAFE.spec.upper_guard_size_bytes == STACK_GUARD_REGION_SIZE);
#[cfg(sanitize = "safestack")]
zr::static_assert!(UNSAFE.spec.lower_guard_size_bytes == STACK_GUARD_REGION_SIZE);
#[cfg(sanitize = "safestack")]
zr::static_assert!(UNSAFE.spec.upper_guard_size_bytes == STACK_GUARD_REGION_SIZE);
#[cfg(sanitize = "shadow-call-stack")]
zr::static_assert!(SHADOW_CALL.spec.lower_guard_size_bytes == STACK_GUARD_REGION_SIZE);
#[cfg(sanitize = "shadow-call-stack")]
zr::static_assert!(SHADOW_CALL.spec.upper_guard_size_bytes == STACK_GUARD_REGION_SIZE);

/// Choose a pattern for the stack canary that hopefully doesn't collide with other patterns that
/// might get used on the stack. Explicitly avoid 0xAA, for example, as it is used for filling
/// uninitialized stack variables.
const STACK_CANARY: u8 = 0xBB;

fn stack_canary_offset(stack: &Stack) -> usize {
    let off = stack.size_bytes as usize / 100
        * core::cmp::min(100, BootOptions::get().stack_canary_percent_free) as usize;
    if stack.growth == Growth::Down { off } else { stack.size_bytes as usize - off - 1 }
}

/// # Safety
///
/// `base` must point to `stack.size_bytes` bytes of mapped, writable memory.
unsafe fn stack_canary_write(stack: &Stack, base: *mut u8) {
    // SAFETY: `stack_canary_offset` is less than `stack.size_bytes` (see the static assertions
    // after the `Stack` constants), and the caller guarantees that `base` points to that many
    // writable bytes.
    unsafe { base.add(stack_canary_offset(stack)).write(STACK_CANARY) };
}

/// # Safety
///
/// `base` must point to `stack.size_bytes` bytes of mapped, readable memory.
unsafe fn stack_canary_check(stack: &Stack, base: *const u8) {
    let off = stack_canary_offset(stack);
    // SAFETY: `off` is less than `stack.size_bytes` (see the static assertions after the `Stack`
    // constants), and the caller guarantees that `base` points to that many readable bytes.
    if unsafe { base.add(off).read() } != STACK_CANARY {
        core::hint::cold_path();
        // TODO(https://fxbug.dev/431001857): Upgrade this to an OOPS once we have either fixed root
        // cause or have more diagnostic data to log.
        kprint!(
            "Canary at offset {} in stack {:s} as base {:p} of size {:#x} was corrupted.\n",
            off,
            stack.name,
            base,
            stack.size_bytes
        );
    }
}

/// Takes a portion of the VMO and maps a kernel stack with one page of padding before and after the
/// mapping.
fn map(
    stack: &Stack,
    vmo: &RefPtr<VmObjectPaged>,
    offset: &mut u64,
    map: &mut Mapping,
) -> Result<(), Status> {
    ltracef!("allocating {}\n", stack.name);

    // assert that this mapping hasn't already be created
    debug_assert!(map.vmar.is_none());

    // get a handle to the root vmar
    let vmar =
        VmAspace::kernel_aspace().root_vmar().and_then(|root| root.as_vm_address_region()).unwrap();

    // create a vmar with enough padding for a page before and after the stack
    let kstack_vmar = vmar.create_sub_vmar(
        0,
        (stack.lower_guard_size_bytes + stack.size_bytes + stack.upper_guard_size_bytes) as usize,
        0,
        flag::CAN_MAP_SPECIFIC | flag::CAN_MAP_READ | flag::CAN_MAP_WRITE,
        stack.name.as_bytes(),
    )?;

    // destroy the vmar if we early abort
    // this will also clean up any mappings that may get placed on the vmar
    let mut vmar_cleanup = zr::defer(|| {
        let _ = kstack_vmar.destroy();
    });

    ltracef!("{} vmar at {:#x}\n", stack.name, kstack_vmar.base().0);

    // create a mapping offset lower_guard_size_bytes into the vmar we created
    let mapping_result = kstack_vmar.create_vm_mapping(
        stack.lower_guard_size_bytes as usize,
        stack.size_bytes as usize,
        0,
        flag::SPECIFIC,
        VmObjectPaged::into_vm_object(vmo.clone()),
        *offset,
        ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
        stack.name.as_bytes(),
    )?;

    ltracef!("{} mapping at {:#x}\n", stack.name, mapping_result.base);

    // fault in all the pages so we dont demand fault in the stack
    mapping_result.mapping.map_range(0, stack.size_bytes as usize, true, false)?;

    // SAFETY: `mapping_result.base` is the base of a writable mapping of `stack.size_bytes` bytes,
    // all of which were just faulted in, that stays mapped while `kstack_vmar` is alive.
    unsafe {
        stack_canary_write(stack, ptr::with_exposed_provenance_mut(mapping_result.base));
    }

    VM_KERNEL_STACK_BYTES.add(stack.size_bytes.into());

    // Cancel the cleanup handler on the vmar since we're about to save a
    // reference to it.
    vmar_cleanup.cancel();
    drop(vmar_cleanup);

    // save the relevant bits
    map.vmar = Some(kstack_vmar);

    // Increase the offset to claim this portion of the VMO.
    *offset += u64::from(stack.size_bytes);

    Ok(())
}

fn unmap(stack: &Stack, map: &mut Mapping) -> Result<(), Status> {
    let Some(vmar) = map.vmar.as_ref() else {
        return Ok(());
    };
    ltracef!("removing vmar at {:#x}\n", vmar.base().0);

    // SAFETY: `map.base()` is the base of the `stack.size_bytes` byte read/write mapping created
    // by `map`, which stays mapped until `vmar` is destroyed below.
    unsafe { stack_canary_check(stack, ptr::with_exposed_provenance(map.base())) };

    vmar.destroy()?;
    map.vmar = None;
    VM_KERNEL_STACK_BYTES.add(-i64::from(stack.size_bytes));
    Ok(())
}

/// Holds the relevant metadata and pointers for an individual mapping.
#[repr(C)]
#[derive(Default)]
struct Mapping {
    vmar: Option<RefPtr<VmAddressRegion>>,
}

// Must match `KernelStack::Mapping` in <vm/kstack.h>.
zr::static_assert!(size_of::<Mapping>() == size_of::<*const c_void>());
zr::static_assert!(align_of::<Mapping>() == align_of::<*const c_void>());

impl Mapping {
    /// Returns the lowest address of the stack, or 0 if it is not mapped.
    #[inline]
    fn base(&self) -> usize {
        // The actual stack mapping starts after the padding.
        if let Some(vmar) = &self.vmar {
            vmar.base().0 + STACK_GUARD_REGION_SIZE as usize
        } else {
            0
        }
    }

    /// Returns the size of the stack, or 0 if it is not mapped.
    #[inline]
    fn size(&self) -> usize {
        // Remove the padding from the vmar to get the actual stack size.
        if let Some(vmar) = &self.vmar {
            vmar.size() - STACK_GUARD_REGION_SIZE as usize * 2
        } else {
            0
        }
    }

    /// Returns the address just past the highest address of the stack, or 0 if it is not mapped.
    #[inline]
    fn top(&self) -> usize {
        self.base() + self.size()
    }

    /// Returns the vmar backing the stack, or null if it is not mapped.
    #[inline]
    fn vmar_ptr(&self) -> *const VmAddressRegion {
        self.vmar.as_ref().map_or(ptr::null(), RefPtr::as_ptr)
    }
}

/// KernelStack encapsulates a kernel stack.
///
/// A kernel stack object is not valid until init() has been successfully
/// called.
///
/// Its layout is shared with `KernelStack` in <vm/kstack.h>.
#[repr(C)]
#[derive(Default)]
pub struct KernelStack {
    main_map: Mapping,

    #[cfg(sanitize = "safestack")]
    unsafe_map: Mapping,

    #[cfg(sanitize = "shadow-call-stack")]
    shadow_call_map: Mapping,
}

impl KernelStack {
    /// Initializes a kernel stack with appropriate overrun padding.
    pub fn init(&mut self) -> Result<(), Status> {
        // Determine the total VMO size we need for all stacks.
        #[allow(unused_mut)]
        let mut vmo_size = SAFE.size_bytes as usize;
        #[cfg(sanitize = "safestack")]
        {
            vmo_size += UNSAFE.size_bytes as usize;
        }

        #[cfg(sanitize = "shadow-call-stack")]
        {
            vmo_size += SHADOW_CALL.size_bytes as usize;
        }

        // Create a VMO for our stacks. Although multiple stacks will be allocated from adjacent
        // blocks of the VMO, they are only referenced by their virtual mapping addresses, and so
        // there is no possibility of over or under run of any stack trampling an adjacent one,
        // they will just fault on the guard regions around the mappings. Similarly the mapping
        // location of each stack is randomized independently, so allocating out of the same VMO
        // provides no correlation to the mapped addresses.
        // Using a single VMO reduces bookkeeping memory overhead with no downside, since all
        // stacks have exactly the same lifetime.
        // TODO(https://fxbug.dev/42079345): VMOs containing kernel stacks for user threads should
        // be linked to the attribution objects of the corresponding processes.
        let stack_vmo = match VmObjectPaged::create(
            pmm::ALLOC_FLAG_ANY,
            VmObjectPaged::ALWAYS_PINNED,
            vmo_size as u64,
        ) {
            Ok(stack_vmo) => stack_vmo,
            Err(status) => {
                ltracef!("error allocating kernel stacks for thread\n");
                return Err(status);
            }
        };
        const KERNEL_STACK_NAME: &[u8] = b"kernel-stack";
        let _ = stack_vmo.set_name(KERNEL_STACK_NAME);

        let mut vmo_offset: u64 = 0;
        map(&SAFE, &stack_vmo, &mut vmo_offset, &mut self.main_map)?;

        #[cfg(sanitize = "safestack")]
        map(&UNSAFE, &stack_vmo, &mut vmo_offset, &mut self.unsafe_map)?;

        #[cfg(sanitize = "shadow-call-stack")]
        map(&SHADOW_CALL, &stack_vmo, &mut vmo_offset, &mut self.shadow_call_map)?;

        Ok(())
    }

    /// Logs the relevant stack memory addresses at the given debug level.
    /// This is useful during a thread dump.
    pub fn dump_info(&self, debug_level: u32) {
        let map_dump = |map: &Mapping, tag: &str| {
            dprintf!(
                debug_level,
                "\t{} base {:#x}, size {:#x}, vmar {:p}\n",
                tag,
                map.base(),
                map.size(),
                map.vmar_ptr()
            );
        };

        map_dump(&self.main_map, "stack");
        #[cfg(sanitize = "safestack")]
        map_dump(&self.unsafe_map, "unsafe_stack");
        #[cfg(sanitize = "shadow-call-stack")]
        map_dump(&self.shadow_call_map, "shadow_call_stack");
    }

    /// Returns the stack to its pre-init() state.
    pub fn teardown(&mut self) -> Result<(), Status> {
        unmap(&SAFE, &mut self.main_map)?;
        #[cfg(sanitize = "safestack")]
        unmap(&UNSAFE, &mut self.unsafe_map)?;
        #[cfg(sanitize = "shadow-call-stack")]
        unmap(&SHADOW_CALL, &mut self.shadow_call_map)?;
        Ok(())
    }

    /// Returns the lowest address of the machine stack.
    #[inline]
    pub fn base(&self) -> usize {
        self.main_map.base()
    }

    /// Returns the address just past the highest address of the machine stack.
    #[inline]
    pub fn top(&self) -> usize {
        self.main_map.top()
    }

    /// Returns the lowest address of the unsafe stack.
    #[cfg(sanitize = "safestack")]
    #[inline]
    pub fn unsafe_base(&self) -> usize {
        self.unsafe_map.base()
    }

    /// Returns the address just past the highest address of the unsafe stack.
    #[cfg(sanitize = "safestack")]
    #[inline]
    pub fn unsafe_top(&self) -> usize {
        self.unsafe_map.top()
    }

    /// Returns the lowest address of the shadow call stack.
    #[cfg(sanitize = "shadow-call-stack")]
    #[inline]
    pub fn shadow_call_base(&self) -> usize {
        self.shadow_call_map.base()
    }

    /// Returns the address just past the highest address of the shadow call stack.
    #[cfg(sanitize = "shadow-call-stack")]
    #[inline]
    pub fn shadow_call_top(&self) -> usize {
        self.shadow_call_map.top()
    }
}

impl Drop for KernelStack {
    /// Destruction will automatically call teardown().
    fn drop(&mut self) {
        let result = self.teardown();
        debug_assert!(result.is_ok(), "KernelStack::teardown returned {:?}", result);
    }
}

// FFI entry points for C++ callers in <vm/kstack.h>.

/// Constructs a `KernelStack` in place, in its pre-init() state.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_construct(stack: &mut MaybeUninit<KernelStack>) {
    stack.write(KernelStack::default());
}

/// Destroys a `KernelStack` in place, calling teardown() first.
///
/// # Safety
///
/// `stack` must point to a valid, initialized `KernelStack` that is not used afterwards.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_kernel_stack_destroy(stack: *mut KernelStack) {
    // SAFETY: The caller guarantees that `stack` points to a valid, initialized `KernelStack`
    // that is not used again.
    unsafe { ptr::drop_in_place(stack) };
}

/// Forwards `KernelStack::Init`.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_init(stack: &mut KernelStack) -> zx_status_t {
    Status::result_into_raw(stack.init())
}

/// Forwards `KernelStack::DumpInfo`.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_dump_info(stack: &KernelStack, debug_level: c_int) {
    // `dprintf` prints every level at or below the enabled one, so a negative level always
    // prints, like `CRITICAL`.
    stack.dump_info(u32::try_from(debug_level).unwrap_or(debug::dprintf::CRITICAL));
}

/// Forwards `KernelStack::Teardown`.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_teardown(stack: &mut KernelStack) -> zx_status_t {
    Status::result_into_raw(stack.teardown())
}

/// Forwards `KernelStack::base`.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_base(stack: &KernelStack) -> usize {
    stack.base()
}

/// Forwards `KernelStack::top`.
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_top(stack: &KernelStack) -> usize {
    stack.top()
}

/// Forwards `KernelStack::unsafe_base`.
#[cfg(sanitize = "safestack")]
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_unsafe_base(stack: &KernelStack) -> usize {
    stack.unsafe_base()
}

/// Forwards `KernelStack::unsafe_top`.
#[cfg(sanitize = "safestack")]
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_unsafe_top(stack: &KernelStack) -> usize {
    stack.unsafe_top()
}

/// Forwards `KernelStack::shadow_call_base`.
#[cfg(sanitize = "shadow-call-stack")]
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_shadow_call_base(stack: &KernelStack) -> usize {
    stack.shadow_call_base()
}

/// Forwards `KernelStack::shadow_call_top`.
#[cfg(sanitize = "shadow-call-stack")]
#[unsafe(no_mangle)]
extern "C" fn rust_kernel_stack_shadow_call_top(stack: &KernelStack) -> usize {
    stack.shadow_call_top()
}
