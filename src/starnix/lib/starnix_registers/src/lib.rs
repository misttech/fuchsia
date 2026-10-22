// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(target_arch = "aarch64")]
mod arm64;

#[cfg(target_arch = "aarch64")]
pub use arm64::*;

#[cfg(target_arch = "x86_64")]
mod x64;

#[cfg(target_arch = "x86_64")]
pub use x64::*;

#[cfg(target_arch = "riscv64")]
mod riscv64;

#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

use starnix_logging::{CATEGORY_STARNIX, NAME_MAP_RESTRICTED_STATE};
use starnix_uapi::__static_assertions::assert_not_impl_any;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::LazyLock;

/// Size of `RESTRICTED_STATE_VMAR`.
///
/// With single-page state VMOs this fits 65536 concurrently bound restricted states, which covers
/// the default Linux `pid_max` (32768). Threads beyond that capacity fall back to mapping their
/// state into the root VMAR.
const RESTRICTED_STATE_VMAR_SIZE: usize = 256 * 1024 * 1024;

/// A dedicated `ZX_VM_COMPACT` sub-VMAR for restricted state mappings.
///
/// Mapping the per-thread state VMOs into a compact sub-VMAR, rather than randomly placing each
/// one in the root VMAR, keeps `zx_vmar_map` cheap and keeps the mappings densely packed in
/// shared page tables. The sub-VMAR itself is randomly placed, but the mappings within it are
/// packed next to each other without gaps.
///
/// This is `None` if the sub-VMAR could not be allocated, in which case all mappings go to the
/// root VMAR.
static RESTRICTED_STATE_VMAR: LazyLock<Option<zx::Vmar>> = LazyLock::new(|| {
    fuchsia_runtime::vmar_root_self()
        .allocate(
            0,
            RESTRICTED_STATE_VMAR_SIZE,
            zx::VmarFlags::CAN_MAP_READ | zx::VmarFlags::CAN_MAP_WRITE | zx::VmarFlags::COMPACT,
        )
        .map(|(vmar, _base)| vmar)
        .ok()
});

/// Maps `state_vmo` for read/write access into `RESTRICTED_STATE_VMAR`, falling back to the root
/// VMAR.
///
/// Returns the mapped address and whether the mapping is in `RESTRICTED_STATE_VMAR` (as opposed
/// to the root VMAR).
fn map_state_vmo(state_vmo: &zx::Vmo, size: usize) -> Result<(usize, bool), zx::Status> {
    map_state_vmo_in(RESTRICTED_STATE_VMAR.as_ref(), state_vmo, size)
}

/// Maps `state_vmo` for read/write access into `compact_vmar`, or into the root VMAR if
/// `compact_vmar` is `None` or full.
///
/// Returns the mapped address and whether the mapping is in `compact_vmar`.
fn map_state_vmo_in(
    compact_vmar: Option<&zx::Vmar>,
    state_vmo: &zx::Vmo,
    size: usize,
) -> Result<(usize, bool), zx::Status> {
    // `zx_restricted_bind_state` commits and pins the state VMO's pages, so `MAP_RANGE` populates
    // the page table entries during the map and the first access below does not page fault.
    let flags = zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE | zx::VmarFlags::MAP_RANGE;
    if let Some(vmar) = compact_vmar {
        match vmar.map(0, state_vmo, 0, size, flags) {
            Ok(address) => return Ok((address, true)),
            // The compact VMAR is full; fall back to the root VMAR.
            Err(zx::Status::NO_RESOURCES) => {}
            Err(status) => return Err(status),
        }
    }
    let address = fuchsia_runtime::vmar_root_self().map(0, state_vmo, 0, size, flags)?;
    Ok((address, false))
}

/// Unmaps a mapping created by `map_state_vmo_in` with the same `compact_vmar`.
///
/// # Safety
///
/// The mapping at `address` must not be accessed after this call.
unsafe fn unmap_state_vmo_in(
    compact_vmar: Option<&zx::Vmar>,
    in_compact_vmar: bool,
    address: usize,
    size: usize,
) -> Result<(), zx::Status> {
    if in_compact_vmar {
        let vmar = compact_vmar.expect("the compact VMAR must exist if it holds a mapping");
        // SAFETY: The caller guarantees that the mapping is no longer accessed.
        unsafe { vmar.unmap(address, size) }
    } else {
        // SAFETY: The caller guarantees that the mapping is no longer accessed.
        unsafe { fuchsia_runtime::vmar_root_self().unmap(address, size) }
    }
}

/// `RestrictedState` manages accesses into the restricted state VMO.
///
/// See `zx_restricted_bind_state`.
pub struct RestrictedState {
    pub bound_state: NonNull<zx::sys::zx_restricted_state_t>,
    state_size: usize,
    /// Whether `bound_state` is mapped in `RESTRICTED_STATE_VMAR` rather than the root VMAR.
    in_compact_vmar: bool,
}

impl RestrictedState {
    /// Allocates a VMO for the restricted state and maps it into the Starnix kernel.
    ///
    /// The VMO is created using `zx_restricted_bind_state` and then mapped into the
    /// kernel's address space. This allows Starnix to inspect and modify the
    /// restricted state (registers) of the task.
    pub fn bind_and_map(
        register_state: &mut RegisterState<RegisterStorageEnum>,
        exception_report: &mut zx::sys::zx_exception_report_t,
    ) -> Result<Self, zx::Status> {
        fuchsia_trace::duration!(CATEGORY_STARNIX, NAME_MAP_RESTRICTED_STATE);
        let mut out_vmo_handle = 0;
        // SAFETY: `out_vmo_handle` is a valid pointer to a handle on the stack.
        let status = zx::Status::ok(unsafe {
            zx::sys::zx_restricted_bind_state(
                0,
                &mut out_vmo_handle,
                std::ptr::from_mut(exception_report),
            )
        });
        match status {
            Ok(()) => {
                // We've successfully attached the VMO to the current thread. This VMO will be
                // mapped and used for the kernel to store restricted mode register state as it
                // enters and exits restricted mode.
            }
            Err(status) => panic!("zx_restricted_bind_state failed with {status}!"),
        }
        // SAFETY: `out_vmo_handle` is a valid handle as `zx_restricted_bind_state` returned OK.
        let state_vmo = unsafe { zx::Vmo::from(zx::NullableHandle::from_raw(out_vmo_handle)) };

        // Name the VMO so external tools (like the CPU profiler) can capture
        // this specific VMO.
        let name = format!(
            "restricted_state_vmo:{}",
            fuchsia_runtime::with_thread_self(|t| t.koid())?.raw_koid()
        );
        let name = zx::Name::new_lossy(&name);
        state_vmo.set_name(&name)?;

        let state_size = state_vmo.get_size()? as usize;
        if state_size < std::mem::size_of::<zx::sys::zx_restricted_exception_t>() {
            return Err(zx::Status::INVALID_ARGS);
        }

        let (state_address, in_compact_vmar) = map_state_vmo(&state_vmo, state_size)?;

        // This memory is not managed by Rust's stack, heap, etc. so treat it as "foreign" memory
        // with no provenance.
        let state_address: *mut zx::sys::zx_restricted_state_t =
            std::ptr::without_provenance_mut(state_address);
        assert!(state_address.is_aligned(), "Zircon must map restricted-state-aligned memory");
        let bound_state =
            NonNull::new(state_address).expect("Zircon must map non-null restricted-state");

        // Copy the initial register state into the mapped VMO and link the VMO to the register
        // state of the current thread.
        // SAFETY: `bound_state` is valid to read/write to as long as `RestrictedState` is live.
        unsafe {
            let vmo_ptr = bound_state.as_ptr();
            vmo_ptr.write(**register_state);
            register_state.real_registers =
                RegisterStorageEnum::Vmo(MappedVmoRegs(NonNull::new_unchecked(vmo_ptr)));
        }

        Ok(Self { state_size, bound_state, in_compact_vmar })
    }
}

impl std::ops::Drop for RestrictedState {
    fn drop(&mut self) {
        let mapping_addr = self.bound_state.as_ptr() as usize;
        // Safety: We are un-mapping the state VMO. This is safe because we route all access
        // into this memory region though this struct so it is safe to unmap on Drop.
        unsafe {
            unmap_state_vmo_in(
                RESTRICTED_STATE_VMAR.as_ref(),
                self.in_compact_vmar,
                mapping_addr,
                self.state_size,
            )
            .expect("Failed to unmap");
            zx::sys::zx_restricted_unbind_state(0);
        }
    }
}

pub trait RegisterStorage:
    std::ops::Deref<Target = zx::sys::zx_restricted_state_t>
    + std::ops::DerefMut
    + Eq
    + PartialEq
    + std::fmt::Debug
    + Clone
{
}

#[derive(Eq, PartialEq, Clone)]
struct MappedVmoRegs(NonNull<zx::sys::zx_restricted_state_t>);
// MappedVmoRegs should be tied to the CurrentTask, so it is not Send or Sync.
assert_not_impl_any!(MappedVmoRegs: Send, Sync);

impl std::fmt::Debug for MappedVmoRegs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MappedVmoRegs").field(&format_args!("{:?}", self.deref())).finish()
    }
}

impl std::ops::Deref for MappedVmoRegs {
    type Target = zx::sys::zx_restricted_state_t;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The pointer is valid and points to a valid `zx_restricted_state_t`.
        unsafe { self.0.as_ref() }
    }
}

impl std::ops::DerefMut for MappedVmoRegs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: The pointer is valid and points to a valid `zx_restricted_state_t`.
        unsafe { self.0.as_mut() }
    }
}

impl RegisterStorage for MappedVmoRegs {}

#[derive(Eq, PartialEq, Debug, Clone, Default)]
pub struct HeapRegs(Box<zx::sys::zx_restricted_state_t>);

impl std::ops::Deref for HeapRegs {
    type Target = zx::sys::zx_restricted_state_t;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for HeapRegs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl RegisterStorage for HeapRegs {}

impl From<RegisterStorageEnum> for HeapRegs {
    fn from(regs: RegisterStorageEnum) -> Self {
        match regs {
            RegisterStorageEnum::Vmo(vmo) => HeapRegs(Box::new(*vmo)),
            RegisterStorageEnum::Heap(heap) => heap,
        }
    }
}

/// An enum to hold the registers in either a heap or a vmo.
///
/// This is introduced to allow `CurrentTask` to store registers in heap during initialization and
/// link to the vmo after the `RestrictedState` is created.
#[derive(Eq, PartialEq, Debug)]
pub enum RegisterStorageEnum {
    // Keep it private to prevent from using it directly.
    #[allow(private_interfaces)]
    Vmo(MappedVmoRegs),
    Heap(HeapRegs),
}
assert_not_impl_any!(RegisterStorageEnum: Send, Sync);

impl std::ops::Deref for RegisterStorageEnum {
    type Target = zx::sys::zx_restricted_state_t;

    fn deref(&self) -> &Self::Target {
        match self {
            RegisterStorageEnum::Vmo(vmo) => vmo.deref(),
            RegisterStorageEnum::Heap(heap) => heap.deref(),
        }
    }
}

impl std::ops::DerefMut for RegisterStorageEnum {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            RegisterStorageEnum::Vmo(vmo) => vmo.deref_mut(),
            RegisterStorageEnum::Heap(heap) => heap.deref_mut(),
        }
    }
}

impl Clone for RegisterStorageEnum {
    fn clone(&self) -> RegisterStorageEnum {
        RegisterStorageEnum::Heap(HeapRegs(Box::new(**self)))
    }
}

impl RegisterStorage for RegisterStorageEnum {}

impl From<MappedVmoRegs> for RegisterStorageEnum {
    fn from(regs: MappedVmoRegs) -> Self {
        RegisterStorageEnum::Vmo(regs)
    }
}

impl From<HeapRegs> for RegisterStorageEnum {
    fn from(regs: HeapRegs) -> Self {
        RegisterStorageEnum::Heap(regs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_size() -> usize {
        zx::system_get_page_size() as usize
    }

    /// Creates a one-page VMO with its page committed, like the VMO returned by
    /// `zx_restricted_bind_state`.
    fn create_state_vmo() -> zx::Vmo {
        let vmo = zx::Vmo::create(page_size() as u64).expect("failed to create VMO");
        vmo.op_range(zx::VmoOp::COMMIT, 0, page_size() as u64).expect("failed to commit VMO");
        vmo
    }

    /// Allocates a compact sub-VMAR with room for exactly one state mapping.
    fn create_one_page_compact_vmar() -> zx::Vmar {
        let (vmar, _base) = fuchsia_runtime::vmar_root_self()
            .allocate(
                0,
                page_size(),
                zx::VmarFlags::CAN_MAP_READ | zx::VmarFlags::CAN_MAP_WRITE | zx::VmarFlags::COMPACT,
            )
            .expect("failed to allocate compact VMAR");
        vmar
    }

    fn vmar_contains(vmar: &zx::Vmar, address: usize) -> bool {
        let info = vmar.info().expect("failed to get VMAR info");
        (info.base..info.base + info.len).contains(&address)
    }

    /// Checks that the mapping at `address` is backed by `vmo` and writable.
    fn check_mapping(vmo: &zx::Vmo, address: usize) {
        let ptr = std::ptr::with_exposed_provenance_mut::<u64>(address);
        // SAFETY: `address` is the start of a live, writable mapping of at least one page.
        unsafe { ptr.write_volatile(0x0123_4567_89ab_cdef) };
        let mut value = [0u8; 8];
        vmo.read(&mut value, 0).expect("failed to read VMO");
        assert_eq!(u64::from_ne_bytes(value), 0x0123_4567_89ab_cdef);
    }

    #[test]
    fn map_state_vmo_uses_restricted_state_vmar() {
        assert!(RESTRICTED_STATE_VMAR.is_some(), "failed to allocate RESTRICTED_STATE_VMAR");
        let vmo = create_state_vmo();
        let (address, in_compact_vmar) =
            map_state_vmo(&vmo, page_size()).expect("failed to map state VMO");
        assert!(in_compact_vmar);
        assert!(vmar_contains(RESTRICTED_STATE_VMAR.as_ref().unwrap(), address));
        check_mapping(&vmo, address);
        // SAFETY: The mapping is not accessed after this.
        unsafe {
            unmap_state_vmo_in(
                RESTRICTED_STATE_VMAR.as_ref(),
                in_compact_vmar,
                address,
                page_size(),
            )
        }
        .expect("failed to unmap state VMO");
    }

    #[test]
    fn map_state_vmo_in_compact_vmar_and_unmap() {
        let compact_vmar = create_one_page_compact_vmar();
        let vmo = create_state_vmo();
        let (address, in_compact_vmar) =
            map_state_vmo_in(Some(&compact_vmar), &vmo, page_size()).expect("failed to map");
        assert!(in_compact_vmar);
        assert!(vmar_contains(&compact_vmar, address));
        check_mapping(&vmo, address);
        // SAFETY: The mapping is not accessed after this.
        unsafe { unmap_state_vmo_in(Some(&compact_vmar), in_compact_vmar, address, page_size()) }
            .expect("failed to unmap");

        // Unmapping freed the only slot in the compact VMAR, so mapping again lands there.
        let (address, in_compact_vmar) =
            map_state_vmo_in(Some(&compact_vmar), &vmo, page_size()).expect("failed to remap");
        assert!(in_compact_vmar);
        // SAFETY: The mapping is not accessed after this.
        unsafe { unmap_state_vmo_in(Some(&compact_vmar), in_compact_vmar, address, page_size()) }
            .expect("failed to unmap");
    }

    #[test]
    fn map_state_vmo_in_falls_back_to_root_vmar_when_compact_vmar_is_full() {
        let compact_vmar = create_one_page_compact_vmar();
        let first_vmo = create_state_vmo();
        let second_vmo = create_state_vmo();

        let (first_address, first_in_compact_vmar) =
            map_state_vmo_in(Some(&compact_vmar), &first_vmo, page_size()).expect("failed to map");
        assert!(first_in_compact_vmar);

        // The compact VMAR is full, so `map` fails with `ZX_ERR_NO_RESOURCES` and the second
        // mapping falls back to the root VMAR.
        let (second_address, second_in_compact_vmar) =
            map_state_vmo_in(Some(&compact_vmar), &second_vmo, page_size()).expect("failed to map");
        assert!(!second_in_compact_vmar);
        assert!(!vmar_contains(&compact_vmar, second_address));
        check_mapping(&second_vmo, second_address);

        // SAFETY: Neither mapping is accessed after this.
        unsafe {
            unmap_state_vmo_in(
                Some(&compact_vmar),
                second_in_compact_vmar,
                second_address,
                page_size(),
            )
            .expect("failed to unmap from the root VMAR");
            unmap_state_vmo_in(
                Some(&compact_vmar),
                first_in_compact_vmar,
                first_address,
                page_size(),
            )
            .expect("failed to unmap from the compact VMAR");
        }
    }

    #[test]
    fn map_state_vmo_in_without_compact_vmar_uses_root_vmar() {
        let vmo = create_state_vmo();
        let (address, in_compact_vmar) =
            map_state_vmo_in(None, &vmo, page_size()).expect("failed to map");
        assert!(!in_compact_vmar);
        check_mapping(&vmo, address);
        // SAFETY: The mapping is not accessed after this.
        unsafe { unmap_state_vmo_in(None, in_compact_vmar, address, page_size()) }
            .expect("failed to unmap");
    }
}
