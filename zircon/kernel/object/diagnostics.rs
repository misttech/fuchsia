// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::io_buffer_dispatcher::IoBufferDispatcher;
use super::process_dispatcher::ProcessDispatcher;
use super::vm_object_dispatcher::{VmObjectDispatcher, VmoOwnership, vmo_to_info_entry};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_INVALID, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_WRITE, ArchMmuFlags,
};
use crate::vm::attribution::{AttributionCounts, FractionalBytes};
use crate::vm::vm_address_region::{VmAddressRegion, VmAspaceGuard, VmEnumerator};
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_mapping::{FlagsRange, VmMapping};
use crate::vm::vm_object::VmObject;
use core::ffi::CStr;
use core::pin::Pin;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{
    ZX_INFO_MAPS_TYPE_ASPACE, ZX_INFO_MAPS_TYPE_MAPPING, ZX_INFO_MAPS_TYPE_VMAR, ZX_MAX_NAME_LEN,
    ZX_VM_PERM_EXECUTE, ZX_VM_PERM_READ, ZX_VM_PERM_WRITE, zx_info_maps_mapping_t, zx_info_maps_t,
    zx_info_vmo_t,
};

#[cfg(console_enabled)]
use super::diagnostics_ffi::{
    dump_all_peer_dispatchers, dump_all_vm_objects, dump_handles_for_koid, dump_job_policies,
    dump_orphaned_mapped_vmos, dump_orphaned_slices, dump_peer_dispatchers_by_koid,
    dump_process_address_space, dump_process_handles, dump_process_memory_usage,
    dump_process_vm_objects, dump_vm_object_cow_tree, kill_process,
};
#[cfg(console_enabled)]
use super::handle::HandleTableArena;
#[cfg(console_enabled)]
use super::job_dispatcher::{JobDispatcher, JobEnumerator};
#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
#[cfg(console_enabled)]
use crate::kernel::thread;
#[cfg(console_enabled)]
use crate::platform_rs::timer::DurationMono;
#[cfg(console_enabled)]
use core::ffi::{c_char, c_int, c_void};
#[cfg(console_enabled)]
use core::ptr;
#[cfg(console_enabled)]
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(console_enabled)]
use kprint::{kformat, kprintln};
#[cfg(console_enabled)]
use zx_types::{
    ZX_OBJ_TYPE_BTI, ZX_OBJ_TYPE_CHANNEL, ZX_OBJ_TYPE_DEBUGLOG, ZX_OBJ_TYPE_EVENT,
    ZX_OBJ_TYPE_EVENTPAIR, ZX_OBJ_TYPE_EXCEPTION, ZX_OBJ_TYPE_FIFO, ZX_OBJ_TYPE_GUEST,
    ZX_OBJ_TYPE_INTERRUPT, ZX_OBJ_TYPE_IOB, ZX_OBJ_TYPE_IOMMU, ZX_OBJ_TYPE_JOB, ZX_OBJ_TYPE_PAGER,
    ZX_OBJ_TYPE_PMT, ZX_OBJ_TYPE_PORT, ZX_OBJ_TYPE_PROCESS, ZX_OBJ_TYPE_PROFILE,
    ZX_OBJ_TYPE_RESOURCE, ZX_OBJ_TYPE_SOCKET, ZX_OBJ_TYPE_SUSPEND_TOKEN, ZX_OBJ_TYPE_THREAD,
    ZX_OBJ_TYPE_TIMER, ZX_OBJ_TYPE_UPPER_BOUND, ZX_OBJ_TYPE_VCPU, ZX_OBJ_TYPE_VMAR,
    ZX_OBJ_TYPE_VMO,
};

/// Abstraction around writing `zx_info_vmo_t`s to user space. As there are multiple versions of the
/// `zx_info_vmo_t` struct and we generally just want to produce the most recent one,
/// implementations of this trait deal with converting from the most recent representation to the
/// version requested by the user.
/// Since conversion may need to occur element by element this provides no multi-element array
/// writes like the regular user_copy interface.
pub trait VmoInfoWriter {
    /// Writes a single `zx_info_vmo_t` at the given element offset. Return values are same as
    /// `user_ptr::copy_to_user`.
    fn write(&mut self, vmo: &zx_info_vmo_t, offset: usize) -> Result<(), Status>;
    /// Increases the base offset such that writes to offset 0 write to this offset.
    fn add_offset(&mut self, offset: usize);
}

/// Similar to [`VmoInfoWriter`], but for `zx_info_maps_t`.
pub trait VmarMapsInfoWriter {
    /// Writes a single `zx_info_maps_t` at the given element offset. Return values are same as
    /// `user_ptr::copy_to_user`.
    fn write(&mut self, maps: &zx_info_maps_t, offset: usize) -> Result<(), Status>;
    /// Increases the base offset such that writes to offset 0 write to this offset.
    fn add_offset(&mut self, offset: usize);
}

fn copy_cstr_to_name(dst: &mut [u8; ZX_MAX_NAME_LEN], src: &CStr) {
    let bytes = src.to_bytes();
    let len = bytes.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&bytes[..len]);
    dst[len..].fill(0);
}

/// Returns the NUL-terminated prefix of a `ZX_MAX_NAME_LEN` name buffer for printing.
#[cfg(console_enabled)]
fn c_name(name: &[u8]) -> &CStr {
    CStr::from_bytes_until_nul(name).unwrap_or(c"")
}

fn arch_mmu_flags_to_vm_flags(arch_mmu_flags: ArchMmuFlags) -> u32 {
    if (arch_mmu_flags & ARCH_MMU_FLAG_INVALID) != 0 {
        return 0;
    }
    let mut ret = 0;
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_READ) != 0 {
        ret |= ZX_VM_PERM_READ;
    }
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
        ret |= ZX_VM_PERM_WRITE;
    }
    if (arch_mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
        ret |= ZX_VM_PERM_EXECUTE;
    }
    ret
}

/// Number of entries that are counted, but not written because the user buffer is already full,
/// between checks for contention on the aspace lock.
///
/// Writing an entry always releases and re-acquires the aspace lock (the entry is built and copied
/// out via `call_unlocked`), which lets other threads of the target process make progress.
/// Counting an entry does not, so once the user buffer is full the remainder of the walk would
/// otherwise hold the aspace lock continuously, stalling page faults and VMAR operations of the
/// target process for the whole duration of the walk (several milliseconds for address spaces with
/// many thousands of mappings). To bound this, the lock is yielded if it is found to be contested
/// every this many counted entries.
const COUNT_ONLY_ENTRIES_BETWEEN_CONTENTION_CHECKS: usize = 32;

/// Called by the enumerators below for every entry that is counted but not written, with `count`
/// being the number of entries enumerated so far. Every
/// `COUNT_ONLY_ENTRIES_BETWEEN_CONTENTION_CHECKS` entries, if the aspace lock held by `guard` is
/// contested, briefly releases it so that the waiting threads can acquire it.
///
/// Dropping the lock is permitted at any point of a `VmEnumerator` callback, since
/// `enumerate_children` pauses its enumerator across each callback, with the usual caveat that
/// objects observed before the lock was dropped may have been unmapped meanwhile.
#[inline]
fn yield_aspace_lock_if_contested(
    count: usize,
    vmar: &VmAddressRegion,
    guard: Pin<&mut VmAspaceGuard<'_>>,
) {
    if count.is_multiple_of(COUNT_ONLY_ENTRIES_BETWEEN_CONTENTION_CHECKS)
        && vmar.lock().raw_mutex().is_contested()
    {
        guard.call_unlocked(|| {});
    }
}

/// Builds a list of all VMARs and mappings in a `VmAspace` or `VmAddressRegion`.
struct AspaceEnumerator<'a, W: VmarMapsInfoWriter> {
    writer: &'a mut W,
    depth_offset: usize,
    max: usize,
    available: usize,
}

impl<'a, W: VmarMapsInfoWriter> AspaceEnumerator<'a, W> {
    fn new(writer: &'a mut W, depth_offset: usize, max: usize, avail_start: usize) -> Self {
        Self { writer, depth_offset, max, available: avail_start }
    }

    fn available(&self) -> usize {
        self.available
    }
}

impl<W: VmarMapsInfoWriter> VmEnumerator for AspaceEnumerator<'_, W> {
    fn on_vm_mapping(
        &mut self,
        map: &VmMapping,
        vmar: &VmAddressRegion,
        depth: u32,
        mut guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        let map_base = map.base().0;
        let map_size = map.size();
        let map_object_offset = map.object_offset();
        let vmo: RefPtr<VmObject> =
            map.vmo_locked(guard.token()).expect("mapping in aspace must have a VMO");
        let mut region_offset = 0;
        let mut entry = zx_info_maps_t::default();

        while region_offset < map_size && map.is_alive_locked(guard.token()) {
            let region_base = map_base + region_offset;
            let protect_range: FlagsRange =
                map.arch_mmu_flags_range_locked(guard.token(), region_base);
            let region_len = (protect_range.region_top as usize) - region_base;
            if self.available < self.max {
                let mmu_flags: ArchMmuFlags = protect_range.mmu_flags;
                let available = self.available;
                let depth_offset = self.depth_offset;
                let writer = &mut *self.writer;
                let mut status = Ok(());
                guard.as_mut().call_unlocked(|| {
                    vmo.get_name(&mut entry.name);
                    entry.base = region_base;
                    entry.size = region_len;
                    entry.depth = (depth as usize) + depth_offset;
                    entry.r#type = ZX_INFO_MAPS_TYPE_MAPPING;
                    let object_offset = map_object_offset + (region_base - map_base) as u64;
                    let counts: AttributionCounts =
                        vmo.get_attributed_memory_in_range(object_offset, region_len as u64);
                    let total_scaled_bytes: FractionalBytes = counts.total_scaled_bytes();
                    entry.u.mapping = zx_info_maps_mapping_t {
                        mmu_flags: arch_mmu_flags_to_vm_flags(mmu_flags),
                        padding1: Default::default(),
                        vmo_koid: vmo.user_id(),
                        vmo_offset: object_offset,
                        committed_bytes: counts.uncompressed_bytes,
                        populated_bytes: counts.total_bytes(),
                        committed_private_bytes: counts.private_uncompressed_bytes,
                        populated_private_bytes: counts.total_private_bytes(),
                        committed_scaled_bytes: counts.scaled_uncompressed_bytes.integral,
                        populated_scaled_bytes: total_scaled_bytes.integral,
                        committed_fractional_scaled_bytes: counts
                            .scaled_uncompressed_bytes
                            .fractional,
                        populated_fractional_scaled_bytes: total_scaled_bytes.fractional,
                    };
                    status = writer.write(&entry, available);
                });
                if status.is_err() {
                    return Status::INVALID_ARGS;
                }
            } else {
                yield_aspace_lock_if_contested(self.available, vmar, guard.as_mut());
            }
            self.available += 1;
            region_offset += region_len;
        }
        Status::NEXT
    }

    fn on_vm_address_region(
        &mut self,
        vmar: &VmAddressRegion,
        depth: u32,
        mut guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        if self.available < self.max {
            let mut entry = zx_info_maps_t::default();
            copy_cstr_to_name(&mut entry.name, vmar.name());
            entry.base = vmar.base().0;
            entry.size = vmar.size();
            entry.depth = (depth as usize) + self.depth_offset;
            entry.r#type = ZX_INFO_MAPS_TYPE_VMAR;
            let available = self.available;
            let writer = &mut *self.writer;
            let mut status = Ok(());
            guard.as_mut().call_unlocked(|| {
                status = writer.write(&entry, available);
            });
            if status.is_err() {
                return Status::INVALID_ARGS;
            }
        } else {
            yield_aspace_lock_if_contested(self.available, vmar, guard.as_mut());
        }
        self.available += 1;

        Status::NEXT
    }
}

/// Walks `target_aspace` and writes entries that describe it into `maps`, up to `max` entries.
/// Returns `(actual, available)`, the number of entries written and the number of entries that
/// could have been written.
pub fn get_vm_aspace_maps(
    target_aspace: &VmAspace,
    maps: &mut impl VmarMapsInfoWriter,
    max: usize,
) -> Result<(usize, usize), Status> {
    if target_aspace.is_destroyed() {
        return Err(Status::BAD_STATE);
    }

    if max > 0 {
        let mut entry = zx_info_maps_t::default();
        // SAFETY: `target_aspace` is a live `VmAspace`, and `VmAspace::rename` is only called
        // during `VmAspace` initialization before the object is published, so no concurrent
        // `rename` can occur.
        let name = unsafe { target_aspace.name_ptr() };
        copy_cstr_to_name(&mut entry.name, name);
        entry.base = target_aspace.base();
        entry.size = target_aspace.size();
        entry.depth = 0;
        entry.r#type = ZX_INFO_MAPS_TYPE_ASPACE;
        if maps.write(&entry, 0).is_err() {
            return Err(Status::INVALID_ARGS);
        }
    }

    let root_vmar = target_aspace.root_vmar().ok_or(Status::BAD_STATE)?;
    let mut ae = AspaceEnumerator::new(maps, 1, max, 1);
    root_vmar.enumerate_children(&mut ae)?;
    let available = ae.available();
    let actual = max.min(available);
    Ok((actual, available))
}

/// Walks `target_vmar` and writes entries that describe it into `maps`, up to `max` entries.
/// Returns `(actual, available)`, the number of entries written and the number of entries that
/// could have been written.
pub fn get_vmar_maps(
    target_vmar: &VmAddressRegion,
    maps: &mut impl VmarMapsInfoWriter,
    max: usize,
) -> Result<(usize, usize), Status> {
    let mut ae = AspaceEnumerator::new(maps, 0, max, 0);
    target_vmar.enumerate_children(&mut ae)?;

    let available = ae.available();
    let actual = max.min(available);
    Ok((actual, available))
}

/// Builds a list of all VMOs mapped into a `VmAspace`.
struct AspaceVmoEnumerator<'a, W: VmoInfoWriter> {
    vmos: &'a mut W,
    max: usize,
    available: usize,
}

impl<'a, W: VmoInfoWriter> AspaceVmoEnumerator<'a, W> {
    fn new(vmos: &'a mut W, max: usize) -> Self {
        Self { vmos, max, available: 0 }
    }

    fn available(&self) -> usize {
        self.available
    }
}

impl<W: VmoInfoWriter> VmEnumerator for AspaceVmoEnumerator<'_, W> {
    fn on_vm_mapping(
        &mut self,
        map: &VmMapping,
        vmar: &VmAddressRegion,
        _depth: u32,
        mut guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        if self.available < self.max {
            let vmo: RefPtr<VmObject> =
                map.vmo_locked(guard.token()).expect("mapping in aspace must have a VMO");
            let available = self.available;
            let vmos = &mut *self.vmos;
            let mut status = Ok(());
            guard.as_mut().call_unlocked(|| {
                let entry: zx_info_vmo_t =
                    vmo_to_info_entry(&vmo, VmoOwnership::Mapping, /*handle_rights=*/ 0);
                status = vmos.write(&entry, available);
            });
            if let Err(err) = status {
                return err;
            }
        } else {
            yield_aspace_lock_if_contested(self.available, vmar, guard.as_mut());
        }
        self.available += 1;
        Status::NEXT
    }
}

/// Walks `target_aspace` and writes entries that describe its mapped VMOs into `vmos`, up to `max`
/// entries. Returns `(actual, available)`, the number of entries written and the number of entries
/// that could have been written.
pub fn get_vm_aspace_vmos(
    target_aspace: &VmAspace,
    vmos: &mut impl VmoInfoWriter,
    max: usize,
) -> Result<(usize, usize), Status> {
    if target_aspace.is_destroyed() {
        return Err(Status::BAD_STATE);
    }

    let root_vmar = target_aspace.root_vmar().ok_or(Status::BAD_STATE)?;

    let mut ave = AspaceVmoEnumerator::new(vmos, max);
    root_vmar.enumerate_children(&mut ave)?;

    let available = ave.available();
    let actual = available.min(max);
    Ok((actual, available))
}

/// For every VMO in `process`'s handle table, writes an entry into `vmos`, up to `max` entries.
/// Returns `(actual, available)`, the number of entries written and the number of entries that
/// could have been written.
pub fn get_process_vmos(
    process: &ProcessDispatcher,
    vmos: &mut impl VmoInfoWriter,
    max: usize,
) -> Result<(usize, usize), Status> {
    let mut actual = 0;
    let mut available = 0;
    // We may see multiple handles to the same VMO, but leave it to userspace to
    // do deduping.
    process.handle_table().for_each_handle_batched(|_handle, rights, disp| {
        if let Some(vmod) = disp.downcast::<VmObjectDispatcher>() {
            available += 1;
            if actual < max {
                let entry = vmo_to_info_entry(vmod.vmo(), VmoOwnership::Handle, rights);
                if vmos.write(&entry, actual).is_err() {
                    return Err(Status::INVALID_ARGS);
                }
                actual += 1;
            }
            return Ok(());
        }
        if let Some(iobd) = disp.downcast::<IoBufferDispatcher>() {
            available += iobd.region_count();
            for i in 0..iobd.region_count() {
                if actual >= max {
                    break;
                }
                let vmo = iobd.get_vmo(i).expect("region index in range");
                let region_map_rights = iobd.get_map_rights(rights, i);
                let entry = vmo_to_info_entry(&vmo, VmoOwnership::IoBuffer, region_map_rights);
                if vmos.write(&entry, actual).is_err() {
                    return Err(Status::INVALID_ARGS);
                }
                actual += 1;
            }
        }
        // We can skip this if it isn't a VMO or IOB
        Ok(())
    })?;
    Ok((actual, available))
}

/// Machinery to walk over a job tree and run a callback on each process.
#[cfg(console_enabled)]
struct ProcessWalker<F: FnMut(&ProcessDispatcher)> {
    cb: F,
}

#[cfg(console_enabled)]
impl<F: FnMut(&ProcessDispatcher)> JobEnumerator for ProcessWalker<F> {
    fn on_process(&mut self, process: &ProcessDispatcher) -> bool {
        (self.cb)(process);
        true
    }
}

#[cfg(console_enabled)]
fn make_process_walker<F: FnMut(&ProcessDispatcher)>(cb: F) -> ProcessWalker<F> {
    ProcessWalker { cb }
}

/// Machinery to walk over a job tree and run a callback on each job.
#[cfg(console_enabled)]
struct JobWalker<F: FnMut(&JobDispatcher)> {
    cb: F,
}

#[cfg(console_enabled)]
impl<F: FnMut(&JobDispatcher)> JobEnumerator for JobWalker<F> {
    fn on_job(&mut self, job: &JobDispatcher) -> bool {
        (self.cb)(job);
        true
    }
}

#[cfg(console_enabled)]
fn make_job_walker<F: FnMut(&JobDispatcher)>(cb: F) -> JobWalker<F> {
    JobWalker { cb }
}

/// Per-`zx_obj_type_t` handle counts, indexed by object type.
#[cfg(console_enabled)]
type HandleTypeCounts<'a> = &'a mut [u32; ZX_OBJ_TYPE_UPPER_BOUND];

/// Returns the count of a process's handles. For each handle, the corresponding
/// `zx_obj_type_t`-indexed element of `handle_types` is incremented.
#[cfg(console_enabled)]
fn build_handle_stats(pd: &ProcessDispatcher, handle_types: HandleTypeCounts<'_>) -> u32 {
    let mut total = 0;
    let _ = pd.handle_table().for_each_handle(|_handle, _rights, disp| {
        let obj_type = disp.get_type() as usize;
        handle_types[obj_type] += 1;
        total += 1;
        Ok(())
    });
    total
}

/// Counts the process's handles by type and formats them into the provided
/// buffer as strings.
#[cfg(console_enabled)]
fn format_handle_type_count<'a>(pd: &ProcessDispatcher, buf: &'a mut [u8]) -> &'a [u8] {
    let mut types = [0u32; ZX_OBJ_TYPE_UPPER_BOUND];
    let handle_count = build_handle_stats(pd, &mut types);

    kformat!(
        buf,
        "{:4u}: {:4u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u} {:3u}",
        handle_count,
        types[ZX_OBJ_TYPE_JOB as usize],
        types[ZX_OBJ_TYPE_PROCESS as usize],
        types[ZX_OBJ_TYPE_THREAD as usize],
        types[ZX_OBJ_TYPE_VMO as usize],
        types[ZX_OBJ_TYPE_VMAR as usize],
        types[ZX_OBJ_TYPE_CHANNEL as usize],
        types[ZX_OBJ_TYPE_EVENT as usize] + types[ZX_OBJ_TYPE_EVENTPAIR as usize],
        types[ZX_OBJ_TYPE_PORT as usize],
        types[ZX_OBJ_TYPE_SOCKET as usize],
        types[ZX_OBJ_TYPE_TIMER as usize],
        types[ZX_OBJ_TYPE_FIFO as usize],
        types[ZX_OBJ_TYPE_INTERRUPT as usize]
            + types[ZX_OBJ_TYPE_DEBUGLOG as usize]
            + types[ZX_OBJ_TYPE_RESOURCE as usize]
            + types[ZX_OBJ_TYPE_GUEST as usize]
            + types[ZX_OBJ_TYPE_VCPU as usize]
            + types[ZX_OBJ_TYPE_IOMMU as usize]
            + types[ZX_OBJ_TYPE_BTI as usize]
            + types[ZX_OBJ_TYPE_PROFILE as usize]
            + types[ZX_OBJ_TYPE_PMT as usize]
            + types[ZX_OBJ_TYPE_SUSPEND_TOKEN as usize]
            + types[ZX_OBJ_TYPE_PAGER as usize]
            + types[ZX_OBJ_TYPE_EXCEPTION as usize]
    )
}

#[cfg(console_enabled)]
fn dump_process_list() {
    kprintln!("{:>7s}   #h:  #jb #pr #th #vo #vm #ch #ev #po #so #tm #fi #?? [name]", "id");

    let mut walker = make_process_walker(|process: &ProcessDispatcher| {
        let mut handle_counts = [0u8; (ZX_OBJ_TYPE_UPPER_BOUND * 4) + 1 + /*slop*/ 16];
        let handle_counts_str = format_handle_type_count(process, &mut handle_counts);

        let mut pname = [0u8; ZX_MAX_NAME_LEN];
        let status = process.get_name(&mut pname);
        debug_assert!(status.is_ok());
        kprintln!("{:7u} {:s} [{:s}]", process.get_koid(), handle_counts_str, c_name(&pname));
    });
    JobDispatcher::get_root_job().enumerate_children_recursive(&mut walker);
}

#[cfg(console_enabled)]
fn dump_job_list() {
    kprintln!("All jobs:");
    kprintln!("{:>7s} {:s}", "koid", "name");
    let mut walker = make_job_walker(|job: &JobDispatcher| {
        let mut name = [0u8; ZX_MAX_NAME_LEN];
        let status = job.get_name(&mut name);
        debug_assert!(status.is_ok());
        kprintln!("{:7u} '{:s}'", job.get_koid(), c_name(&name));
    });
    JobDispatcher::get_root_job().enumerate_children_recursive(&mut walker);
}

#[cfg(console_enabled)]
const MB: usize = 1024 * 1024;

#[cfg(console_enabled)]
static MWD_LIMIT_BYTES: AtomicUsize = AtomicUsize::new(32 * MB);
#[cfg(console_enabled)]
static MWD_RUNNING: AtomicBool = AtomicBool::new(false);

#[cfg(console_enabled)]
static HWD_LIMIT: AtomicUsize = AtomicUsize::new(1024);
#[cfg(console_enabled)]
static HWD_RUNNING: AtomicBool = AtomicBool::new(false);

#[cfg(console_enabled)]
fn dump_process_list_key_map() {
    kprintln!("id  : process id number");
    kprintln!("#h  : total number of handles");
    kprintln!("#jb : number of job handles");
    kprintln!("#pr : number of process handles");
    kprintln!("#th : number of thread handles");
    kprintln!("#vo : number of vmo handles");
    kprintln!("#vm : number of virtual memory address region handles");
    kprintln!("#ch : number of channel handles");
    kprintln!("#ev : number of event and event pair handles");
    kprintln!("#po : number of port handles");
    kprintln!("#so: number of sockets");
    kprintln!("#tm : number of timers");
    kprintln!("#fi : number of fifos");
    kprintln!("#?? : number of all other handle types");
}

#[cfg(console_enabled)]
fn dump_handle_rights_key_map() {
    kprintln!("dup : ZX_RIGHT_DUPLICATE");
    kprintln!("tr  : ZX_RIGHT_TRANSFER");
    kprintln!("r   : ZX_RIGHT_READ");
    kprintln!("w   : ZX_RIGHT_WRITE");
    kprintln!("x   : ZX_RIGHT_EXECUTE");
    kprintln!("map : ZX_RIGHT_MAP");
    kprintln!("gpr : ZX_RIGHT_GET_PROPERTY");
    kprintln!("spr : ZX_RIGHT_SET_PROPERTY");
    kprintln!("enm : ZX_RIGHT_ENUMERATE");
    kprintln!("des : ZX_RIGHT_DESTROY");
    kprintln!("spo : ZX_RIGHT_SET_POLICY");
    kprintln!("gpo : ZX_RIGHT_GET_POLICY");
    kprintln!("sig : ZX_RIGHT_SIGNAL");
    kprintln!("sigp: ZX_RIGHT_SIGNAL_PEER");
    kprintln!("wt  : ZX_RIGHT_WAIT");
    kprintln!("ins : ZX_RIGHT_INSPECT");
    kprintln!("mj  : ZX_RIGHT_MANAGE_JOB");
    kprintln!("mp  : ZX_RIGHT_MANAGE_PROCESS");
    kprintln!("mt  : ZX_RIGHT_MANAGE_THREAD");
    kprintln!("ap  : ZX_RIGHT_APPLY_PROFILE");
    kprintln!("ms  : ZX_RIGHT_MANAGE_SOCKET");
}

/// Dumps an address space based on the arg.
#[cfg(console_enabled)]
fn dump_address_space(arg: &CmdArgs) {
    if "kernel".starts_with(arg.as_str()) {
        // The arg is a prefix of "kernel".
        VmAspace::kernel_aspace().dump(true);
    } else {
        dump_process_address_space(arg.arg_uint);
    }
}

#[cfg(console_enabled)]
fn dump_handle_table() {
    kprintln!("outstanding handles: {}", HandleTableArena::diagnostic_count());
    HandleTableArena::dump();
}

#[cfg(console_enabled)]
extern "C" fn hwd_thread(_arg: *mut c_void) -> i32 {
    static PREVIOUS_HANDLE_COUNT: AtomicUsize = AtomicUsize::new(0);

    loop {
        let handle_count = HandleTableArena::diagnostic_count();
        let prev = PREVIOUS_HANDLE_COUNT.load(Ordering::Relaxed);
        if handle_count != prev {
            let limit = HWD_LIMIT.load(Ordering::Relaxed);
            if handle_count > limit {
                kprintln!(
                    "HandleWatchdog! {} handles outstanding (greater than limit {})",
                    handle_count,
                    limit
                );
            } else if prev > limit {
                kprintln!(
                    "HandleWatchdog! {} handles outstanding (dropping below limit {})",
                    handle_count,
                    limit
                );
            }
        }

        PREVIOUS_HANDLE_COUNT.store(handle_count, Ordering::Relaxed);

        let _ = thread::sleep_relative(DurationMono::from_seconds(1));
    }
}

#[cfg(console_enabled)]
extern "C" fn mwd_thread(_arg: *mut c_void) -> i32 {
    loop {
        let _ = thread::sleep_relative(DurationMono::from_seconds(1));
        dump_process_memory_usage(c"MemoryHog! ", MWD_LIMIT_BYTES.load(Ordering::Relaxed));
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_diagnostics(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    let rc: c_int = 0;
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    'usage: {
        if argc < 2 {
            kprintln!("not enough arguments:");
            break 'usage;
        }

        if argv[1].as_str() == "mwd" {
            if argc == 3 {
                MWD_LIMIT_BYTES
                    .store((argv[2].arg_uint as usize).wrapping_mul(MB), Ordering::Relaxed);
            }
            if !MWD_RUNNING.load(Ordering::Relaxed) {
                // SAFETY: `c"mwd".as_ptr()` is a valid NUL-terminated C string, and `mwd_thread`
                // does not dereference its null `arg`.
                if let Ok(t) =
                    unsafe { thread::create(c"mwd".as_ptr(), mwd_thread, ptr::null_mut()) }
                {
                    MWD_RUNNING.store(true, Ordering::Relaxed);
                    // SAFETY: `t` was just created and has not been resumed or joined.
                    unsafe { t.resume() };
                }
            }
        } else if argv[1].as_str() == "ps" {
            if argc == 3 && argv[2].as_str() == "help" {
                dump_process_list_key_map();
            } else {
                dump_process_list();
            }
        } else if argv[1].as_str() == "jobs" {
            dump_job_list();
        } else if argv[1].as_str() == "jobpol" {
            if argc < 3 {
                break 'usage;
            }
            dump_job_policies(argv[2].arg_uint);
        } else if argv[1].as_str() == "hwd" {
            if argc == 3 {
                HWD_LIMIT.store(argv[2].arg_uint as usize, Ordering::Relaxed);
            }
            if !HWD_RUNNING.load(Ordering::Relaxed) {
                // SAFETY: `c"hwd".as_ptr()` is a valid NUL-terminated C string, and `hwd_thread`
                // does not dereference its null `arg`.
                if let Ok(t) =
                    unsafe { thread::create(c"hwd".as_ptr(), hwd_thread, ptr::null_mut()) }
                {
                    HWD_RUNNING.store(true, Ordering::Relaxed);
                    // SAFETY: `t` was just created and has not been resumed or joined.
                    unsafe { t.resume() };
                }
            }
        } else if argv[1].as_str() == "ht" {
            if argc < 3 {
                break 'usage;
            }
            dump_process_handles(argv[2].arg_uint);
        } else if argv[1].as_str() == "ch" {
            if argc == 3 {
                dump_peer_dispatchers_by_koid(ZX_OBJ_TYPE_CHANNEL, argv[2].arg_uint);
            } else {
                dump_all_peer_dispatchers(ZX_OBJ_TYPE_CHANNEL);
            }
        } else if argv[1].as_str() == "sock" {
            if argc == 3 {
                dump_peer_dispatchers_by_koid(ZX_OBJ_TYPE_SOCKET, argv[2].arg_uint);
            } else {
                dump_all_peer_dispatchers(ZX_OBJ_TYPE_SOCKET);
            }
        } else if argv[1].as_str() == "fifo" {
            if argc == 3 {
                dump_peer_dispatchers_by_koid(ZX_OBJ_TYPE_FIFO, argv[2].arg_uint);
            } else {
                dump_all_peer_dispatchers(ZX_OBJ_TYPE_FIFO);
            }
        } else if argv[1].as_str() == "eventpair" {
            if argc == 3 {
                dump_peer_dispatchers_by_koid(ZX_OBJ_TYPE_EVENTPAIR, argv[2].arg_uint);
            } else {
                dump_all_peer_dispatchers(ZX_OBJ_TYPE_EVENTPAIR);
            }
        } else if argv[1].as_str() == "iob" {
            if argc == 3 {
                dump_peer_dispatchers_by_koid(ZX_OBJ_TYPE_IOB, argv[2].arg_uint);
            } else {
                dump_all_peer_dispatchers(ZX_OBJ_TYPE_IOB);
            }
        } else if argv[1].as_str() == "vmos" {
            if argc < 3 {
                break 'usage;
            }
            let mut format_unit: c_char = 0;
            if argc >= 4 {
                if let Some(rest) = argv[3].as_str().strip_prefix("-u") {
                    format_unit = if rest.is_empty() { 0 } else { rest.as_bytes()[0] as c_char };
                } else {
                    kprintln!("dunno '{:cs}'", argv[3].arg_str);
                    break 'usage;
                }
            }
            if argv[2].as_str() == "all" {
                dump_all_vm_objects(false, format_unit);
            } else if argv[2].as_str() == "hidden" {
                dump_all_vm_objects(true, format_unit);
            } else if argv[2].as_str() == "orphaned" {
                dump_orphaned_slices(format_unit);
                dump_orphaned_mapped_vmos(format_unit);
            } else {
                dump_process_vm_objects(argv[2].arg_uint, format_unit);
            }
        } else if argv[1].as_str() == "cow-tree" {
            if argc < 3 {
                break 'usage;
            }
            dump_vm_object_cow_tree(argv[2].arg_uint);
        } else if argv[1].as_str() == "kill" {
            if argc < 3 {
                break 'usage;
            }
            kill_process(argv[2].arg_uint);
        } else if argv[1].as_str() == "asd" {
            if argc < 3 {
                break 'usage;
            }
            dump_address_space(&argv[2]);
        } else if argv[1].as_str() == "htinfo" {
            if argc != 2 {
                break 'usage;
            }
            dump_handle_table();
        } else if argv[1].as_str() == "koid" {
            if argc < 3 {
                break 'usage;
            }

            if argv[2].as_str() == "help" {
                dump_handle_rights_key_map();
            } else {
                dump_handles_for_koid(argv[2].arg_uint);
            }
        } else {
            kprintln!("unrecognized subcommand '{:cs}'", argv[1].arg_str);
            break 'usage;
        }
        return rc;
    }

    kprintln!("{:cs} ps                : list processes", argv[0].arg_str);
    kprintln!(
        "{:cs} ps help           : print header label descriptions for 'ps'",
        argv[0].arg_str
    );
    kprintln!("{:cs} jobs              : list jobs", argv[0].arg_str);
    kprintln!("{:cs} jobpol <koid>     : print policies for given job", argv[0].arg_str);
    kprintln!("{:cs} mwd  <mb>         : memory watchdog", argv[0].arg_str);
    kprintln!("{:cs} ht   <pid>        : dump process handles", argv[0].arg_str);
    kprintln!("{:cs} hwd  <count>      : handle watchdog", argv[0].arg_str);
    kprintln!("{:cs} vmos <pid>|all|hidden|orphaned [-u?]", argv[0].arg_str);
    kprintln!("                     : dump process/all/hidden/orphaned VMOs");
    kprintln!("                 -u? : fix all sizes to the named unit");
    kprintln!("                       where ? is one of [BkMGTPE]");
    kprintln!(
        "{:cs} cow-tree <vmo>    : dump the copy-on-write tree for a vmo koid",
        argv[0].arg_str
    );
    kprintln!("{:cs} kill <pid>        : kill process", argv[0].arg_str);
    kprintln!("{:cs} asd  <pid>|kernel : dump process/kernel address space", argv[0].arg_str);
    kprintln!("{:cs} htinfo            : handle table info", argv[0].arg_str);
    kprintln!("{:cs} koid <koid>       : list all handles for a koid", argv[0].arg_str);
    kprintln!(
        "{:cs} koid help         : print header label descriptions for 'koid'",
        argv[0].arg_str
    );
    kprintln!(
        "{:cs} ch   <koid>       : dump channels for pid or for all processes,",
        argv[0].arg_str
    );
    kprintln!("                       or processes for channel koid");
    kprintln!(
        "{:cs} sock <koid>       : dump sockets for pid or for all processes,",
        argv[0].arg_str
    );
    kprintln!("                       or processes for socket koid");
    kprintln!(
        "{:cs} fifo <koid>       : dump fifos for pid or for all processes,",
        argv[0].arg_str
    );
    kprintln!("                       or processes for fifo koid");
    kprintln!(
        "{:cs} eventpair <koid>  : dump event pairs for pid or for all processes,",
        argv[0].arg_str
    );
    kprintln!("                       or processes for eventpair koid");
    kprintln!(
        "{:cs} iob <koid>        : dump io buffers for pid or for all processes,",
        argv[0].arg_str
    );
    kprintln!("                       or processes for io buffer koid");
    -1
}

#[cfg(console_enabled)]
static_command!(
    CMD_ZX,
    c"zx".as_ptr(),
    c"kernel object diagnostics".as_ptr(),
    cmd_diagnostics,
    CMD_AVAIL_NORMAL
);
