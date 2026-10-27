// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub mod elf;
pub mod resource;
pub mod userabi_ffi;
pub mod vdso;

pub use userabi_ffi::HandoffEnd;
pub use vdso::VDso;

use crate::counters::{
    ARENA_VMO_NAME, CounterArena, CounterDesc, DESCRIPTOR_VMO_NAME, define_kcounter,
};
use crate::crashlog::crashlog_stash;
use crate::instrumentation::InstrumentationData;
use crate::object::{
    ChannelDispatcher, HandleOwner, InitialMutability, JobDispatcher, KernelHandle, LogDispatcher,
    MessagePacket, ProcessDispatcher, ThreadDispatcher, VmAddressRegionDispatcher,
    VmObjectDispatcher, get_root_job_handle, start_root_job_observer,
};
use crate::platform_rs::timer::{current_mono_time, timer_current_mono_ticks};
use crate::vm::pmm::{ALLOC_FLAG_ANY, ALLOC_FLAG_CAN_WAIT};
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;
use boot_options::BootOptions;
use core::cmp::min;
use core::ffi::c_void;
use core::mem;
use debug::{dprintf, kernel_oops};
use elf::{initial_stack_pointer, map_handoff_elf};
use fbl::{RefPtr, Vector};
use page::SIZE as PAGE_SIZE;
use resource::get_resource_handle;
#[cfg(all(target_arch = "aarch64", debug_assertions))]
use userabi_ffi::arm64_print_midr_cpu_name;
use userabi_ffi::{File, HandoffEndElf, PlatformCrashlog, boot_options_show};
#[cfg(enable_entropy_collector_test)]
use userabi_ffi::{entropy_vmo, entropy_vmo_stream_size, entropy_was_lost};
use zr::{defer, slice_from_raw_parts_mut};
use zx_status::Status;
#[cfg(target_arch = "x86_64")]
use zx_types::ZX_RSRC_KIND_IOPORT;
#[cfg(target_arch = "aarch64")]
use zx_types::ZX_RSRC_KIND_SMC;
use zx_types::{
    ZX_KOID_INVALID, ZX_RIGHT_EXECUTE, ZX_RIGHT_WRITE, ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO,
    ZX_RSRC_KIND_SYSTEM, ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL, ZX_VM_CAN_MAP_READ,
    ZX_VM_CAN_MAP_SPECIFIC, ZX_VM_CAN_MAP_WRITE, ZX_VM_PERM_READ, ZX_VM_PERM_WRITE, ZX_VM_SPECIFIC,
    zx_vaddr_t,
};

pub struct VmoBuffer {
    offset: usize,
    size: usize,
    vmo: RefPtr<VmObjectPaged>,
}

impl VmoBuffer {
    fn from_vmo(vmo: RefPtr<VmObjectPaged>) -> Self {
        Self { offset: 0, size: vmo.size() as usize, vmo }
    }

    fn new() -> Result<Self, Status> {
        let vmo =
            VmObjectPaged::create(ALLOC_FLAG_ANY, VmObjectPaged::RESIZABLE, PAGE_SIZE as u64)?;
        let size = vmo.size() as usize;
        Ok(Self { offset: 0, size, vmo })
    }

    pub fn write(&mut self, str: &[u8]) -> i32 {
        // Enlarge the VMO as needed if it was created as resizable.
        if str.len() > self.size - self.offset && self.vmo.is_resizable() {
            debug_assert!((self.vmo.size() as usize) < self.offset + str.len());
            let minimum_size = self.offset + str.len();
            let page_aligned_size = minimum_size.next_multiple_of(PAGE_SIZE);
            let status = self.vmo.resize(page_aligned_size as u64);
            if status.is_ok() {
                // Update unlocked cache of the size.
                self.size = self.vmo.size() as usize;
            } else if self.offset == self.size {
                // None left to write without the resize.
                // Otherwise proceed to write what can be written.
                return Status::result_into_raw(status);
            }
        }

        let todo = min(str.len(), self.size - self.offset);

        if let Err(res) = self.vmo.write(self.offset as u64, &str[..todo]) {
            debug_assert!(res.into_raw() < 0);
            return res.into_raw();
        }

        self.offset += todo;
        debug_assert!(todo <= i32::MAX as usize);
        todo as i32
    }

    fn vmo(&self) -> &RefPtr<VmObjectPaged> {
        &self.vmo
    }

    fn stream_size(&self) -> usize {
        self.offset
    }
}

const STACK_VMO_NAME: &[u8] = b"userboot-initial-stack";
const CRASHLOG_VMO_NAME: &[u8] = b"crashlog";
const BOOT_OPTIONS_VMO_NAME: &[u8] = b"boot-options.txt";

define_kcounter!(TIMELINE_USERBOOT, "boot.timeline.userboot", Sum);
define_kcounter!(INIT_TIME, "init.userboot.time.msec", Sum);

struct Mapped {
    userboot_vmar: HandleOwner,
    userboot_entry: zx_vaddr_t,
    vdso_base: zx_vaddr_t,
    stack_size: usize,
}

struct Userboot {
    userboot_elf: Option<HandoffEndElf>,
    vdso_elf: Option<HandoffEndElf>,
    entry: zx_vaddr_t,
    sp: usize,
    vdso_base: zx_vaddr_t,
}

impl Userboot {
    fn new(userboot: HandoffEndElf, vdso: HandoffEndElf) -> Self {
        Self { userboot_elf: Some(userboot), vdso_elf: Some(vdso), entry: 0, sp: 0, vdso_base: 0 }
    }

    fn map(&mut self, root_vmar: &VmAddressRegionDispatcher) -> Result<HandleOwner, Status> {
        // Map in the userboot image along with the vDSO.
        let mapped = self.map_elf_and_vdso(root_vmar)?;
        dprintf!(SPEW, "userboot: {:<31} @  {:#x}\n", "entry point", mapped.userboot_entry);

        // Set up the stack.
        let sp = Self::map_stack(root_vmar, mapped.stack_size)?;

        self.entry = mapped.userboot_entry;
        self.sp = sp;
        self.vdso_base = mapped.vdso_base;
        Ok(mapped.userboot_vmar)
    }

    fn start(
        &self,
        process: &ProcessDispatcher,
        thread: RefPtr<ThreadDispatcher>,
        arg_handle: Option<HandleOwner>,
    ) -> Result<(), Status> {
        // Start the process running.
        process.start(thread, self.entry, self.sp, arg_handle, self.vdso_base)
    }

    fn map_elf_and_vdso(
        &mut self,
        root_vmar: &VmAddressRegionDispatcher,
    ) -> Result<Mapped, Status> {
        // Map userboot proper.
        let userboot = map_handoff_elf(self.userboot_elf.take().unwrap(), root_vmar)?;

        // Map the vDSO.
        let vdso = map_handoff_elf(self.vdso_elf.take().unwrap(), root_vmar)?;

        let stack_size = userboot.stack_size.ok_or(Status::INVALID_ARGS)?;
        Ok(Mapped {
            userboot_vmar: userboot.vmar,
            userboot_entry: userboot.entry,
            vdso_base: vdso.vaddr_start,
            stack_size,
        })
    }

    // Map the stack anywhere, in its own VMAR and a one-page guard region below.
    fn map_stack(
        root_vmar: &VmAddressRegionDispatcher,
        stack_size: usize,
    ) -> Result<usize, Status> {
        let stack_vmo =
            VmObjectPaged::create(ALLOC_FLAG_ANY | ALLOC_FLAG_CAN_WAIT, 0, stack_size as u64)?;
        let _ = stack_vmo.set_name(STACK_VMO_NAME);

        let vmar_size = stack_size + PAGE_SIZE;
        let (vmar_handle, _vmar_rights) = root_vmar.allocate(
            0,
            vmar_size,
            ZX_VM_CAN_MAP_READ | ZX_VM_CAN_MAP_WRITE | ZX_VM_CAN_MAP_SPECIFIC,
        )?;

        let stack_base = vmar_handle
            .dispatcher()
            .map(
                PAGE_SIZE,
                VmObjectPaged::into_vm_object(stack_vmo.clone()),
                0,
                stack_size,
                ZX_VM_PERM_READ | ZX_VM_PERM_WRITE | ZX_VM_SPECIFIC,
            )?
            .base;
        let sp = initial_stack_pointer(stack_base, stack_size);
        dprintf!(
            SPEW,
            "userboot: {:<31} @ [{:#x}, {:#x})\n",
            "stack mapped",
            stack_base,
            stack_base + stack_size
        );
        let hex_width = |x: usize| 2 + ((usize::BITS - x.leading_zeros()) as usize).div_ceil(4);
        let width = hex_width(stack_base) + 3 + hex_width(sp);
        dprintf!(SPEW, "userboot: {:<31} @ {:#width$x}\n", "sp", sp, width = width);

        let (_vmo_handle, _vmo_rights) =
            VmObjectDispatcher::create(&stack_vmo, stack_size as u64, InitialMutability::Mutable)?;

        Ok(sp)
    }
}

// Get a handle to a VM object, with full rights except perhaps for writing.
fn get_vmo_handle(
    vmo: Option<&VmObject>,
    readonly: bool,
    stream_size: u64,
) -> Result<HandleOwner, Status> {
    let vmo = vmo.ok_or(Status::NO_MEMORY)?;

    let (vmo_kernel_handle, mut rights) =
        VmObjectDispatcher::create(vmo, stream_size, InitialMutability::Mutable)?;
    if readonly {
        rights &= !ZX_RIGHT_WRITE;
    }
    HandleOwner::make(vmo_kernel_handle, rights).ok_or(Status::NO_MEMORY)
}

fn get_job_handle() -> Option<HandleOwner> {
    get_root_job_handle().dup(JobDispatcher::default_rights()).ok()
}

// Converts platform crashlog into a VMO
fn crashlog_to_vmo() -> Result<(RefPtr<VmObject>, usize), Status> {
    let crashlog = PlatformCrashlog::get();

    let size = crashlog.recover(None);
    let aligned_size = VmObject::round_size(size as u64)?;
    let crashlog_vmo = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, aligned_size)?;

    if size != 0 {
        let mut vmo_buffer = VmoBuffer::from_vmo(crashlog_vmo.clone());
        let mut vmo_file = File::new(&mut vmo_buffer);
        crashlog.recover(Some(&mut vmo_file));
    }

    let _ = crashlog_vmo.set_name(CRASHLOG_VMO_NAME);

    // Stash the recovered crashlog so that it may be inspected later from the
    // kernel console.
    crashlog_stash(Some(VmObjectPaged::into_vm_object(crashlog_vmo.clone())));

    // Now that we have recovered the old crashlog, enable crashlog uptime
    // updates.  This will cause systems with a RAM based crashlog to periodically
    // create a payload-less crashlog indicating a SW reboot reason of "unknown"
    // along with an uptime indicator.  If the system spontaneously reboots (due
    // to something like a WDT, or brownout) we will be able to recover this log
    // and know that we spontaneously rebooted, and have some idea of how long we
    // were running before we did.
    crashlog.enable_crashlog_uptime_updates(true);
    Ok((VmObjectPaged::into_vm_object(crashlog_vmo), size))
}

fn bootstrap_vmos(
    handoff_end: HandoffEnd,
    handles: &mut Vector<Option<HandleOwner>>,
) -> Result<Userboot, Status> {
    let mut push_handle = |handle: HandleOwner| {
        let res = handles.push_back(Some(handle));
        assert!(res.is_ok());
    };

    // ZBI VMO
    if let Some(zbi) = handoff_end.zbi {
        push_handle(zbi);
    }

    // vDSO VMOs & TimeValues
    let (vdso, mut vdso_kernel_handles, time_values_handle) = VDso::create(&handoff_end.vdso);

    let time_values = HandleOwner::make(time_values_handle, vdso.vmo_rights() & !ZX_RIGHT_EXECUTE)
        .ok_or(Status::NO_MEMORY)?;
    push_handle(time_values);

    if BootOptions::get().always_use_next_vdso {
        vdso_kernel_handles.swap(0, 1);
    }
    for vdso_kernel_handle in vdso_kernel_handles {
        let vdso_h =
            HandleOwner::make(vdso_kernel_handle, vdso.vmo_rights()).ok_or(Status::NO_MEMORY)?;
        push_handle(vdso_h);
    }

    // Crashlog
    let (crashlog_vmo, crashlog_size) = crashlog_to_vmo()?;
    let crashlog_handle = get_vmo_handle(Some(&crashlog_vmo), true, crashlog_size as u64)?;
    push_handle(crashlog_handle);

    // Boot options
    {
        let mut boot_options = VmoBuffer::new()?;
        let mut boot_options_file = File::new(&mut boot_options);
        boot_options_show(/*defaults=*/ false, &mut boot_options_file);
        let _ = boot_options.vmo().set_name(BOOT_OPTIONS_VMO_NAME);
        let boot_options_handle =
            get_vmo_handle(Some(boot_options.vmo()), false, boot_options.stream_size() as u64)?;
        push_handle(boot_options_handle);
    }

    #[cfg(enable_entropy_collector_test)]
    {
        if entropy_was_lost() {
            return Err(Status::NO_MEMORY);
        }
        let entropy_handle =
            get_vmo_handle(entropy_vmo().as_deref(), true, entropy_vmo_stream_size())?;
        push_handle(entropy_handle);
    }

    // kcounters names table
    let kcountdesc_vmo = VmObjectPaged::create_from_wired_pages(CounterDesc.vmo_data(), true)?;
    let _ = kcountdesc_vmo.set_name(DESCRIPTOR_VMO_NAME);
    let kcountdesc_handle =
        get_vmo_handle(Some(&kcountdesc_vmo), true, CounterDesc.vmo_stream_size() as u64)?;
    push_handle(kcountdesc_handle);

    // kcounters live data
    let kcounters_vmo = VmObjectPaged::create_from_wired_pages(CounterArena.vmo_data(), false)?;
    // Leak a reference to the kcounters VMO so that the kcounters memory always
    // remains valid, even if userspace closes the last handle.
    mem::forget(kcounters_vmo.clone());
    let _ = kcounters_vmo.set_name(ARENA_VMO_NAME);
    let kcounters_handle =
        get_vmo_handle(Some(&kcounters_vmo), true, CounterArena.vmo_stream_size() as u64)?;
    push_handle(kcounters_handle);

    // midr.txt
    {
        const MIDR_TXT: &[u8] = b"midr.txt";
        let mut midr_txt = VmoBuffer::new()?;
        let mut midr_txt_file = File::new(&mut midr_txt);
        #[cfg(all(target_arch = "aarch64", debug_assertions))]
        arm64_print_midr_cpu_name(&mut midr_txt_file);
        #[cfg(not(all(target_arch = "aarch64", debug_assertions)))]
        let _ = &mut midr_txt_file;
        if midr_txt.stream_size() > 0 {
            let _ = midr_txt.vmo().set_name(MIDR_TXT);
        }
        let midr_handle =
            get_vmo_handle(Some(midr_txt.vmo()), false, midr_txt.stream_size() as u64)?;
        push_handle(midr_handle);
    }

    // Instrumentation VMOs
    let mut inst_handles = [const { None }; InstrumentationData::vmo_count()];
    // SAFETY: `bootstrap_vmos` runs once during `userboot_init` at the end of kernel boot handoff,
    // after `LK_INIT_LEVEL_KERNEL` (`InitSancov`) and VM/heap initialization have completed.
    unsafe { InstrumentationData::get_vmos(&mut inst_handles) }?;
    for h in inst_handles.into_iter().flatten() {
        push_handle(h);
    }

    // Extra phys VMOs
    for extra_vmo in handoff_end.extra_phys_vmos.into_iter().flatten() {
        push_handle(extra_vmo);
    }

    Ok(Userboot::new(handoff_end.userboot, handoff_end.vdso))
}

trait MessagePacketHandlesExt {
    fn mutable_handles(&mut self) -> &mut [*mut c_void];
}

impl MessagePacketHandlesExt for MessagePacket {
    fn mutable_handles(&mut self) -> &mut [*mut c_void] {
        let ptr = self.handles_mut();
        let len = self.num_handles();
        // SAFETY: `ptr` points to `len` handle pointer slots in the packet's buffer chain when
        // `len > 0`. Zero-initializing ensures all slots are valid null pointers before creating
        // a mutable slice or dropping on early error return.
        unsafe {
            if len > 0 {
                ptr.write_bytes(0, len);
            }
            slice_from_raw_parts_mut(ptr, len)
        }
    }
}

struct BootstrapChannel {
    user_handle: Option<HandleOwner>,
    send: RefPtr<ChannelDispatcher>,
}

impl BootstrapChannel {
    fn create() -> Result<Self, Status> {
        // Make the channel that will hold the message.
        let (user_handle, kernel_handle, channel_rights) = ChannelDispatcher::create()?;
        let user_handle =
            HandleOwner::make(user_handle, channel_rights).ok_or(Status::NO_MEMORY)?;
        Ok(Self { user_handle: Some(user_handle), send: kernel_handle.release() })
    }

    // Send a message containing only handles.
    fn send_handles(&self, handles: &mut [Option<HandleOwner>]) -> Result<(), Status> {
        let count = handles.len();
        let mut msg = MessagePacket::create_from_kernel(&[], count)?;
        msg.set_owns_handles(true);
        let msg_handles = msg.mutable_handles();
        debug_assert!(msg_handles.len() == count);
        for (slot, handle) in msg_handles.iter_mut().zip(handles.iter_mut()) {
            *slot = handle.take().ok_or(Status::BAD_HANDLE)?.release().as_ptr().cast();
        }
        self.send.write(ZX_KOID_INVALID, msg)
    }

    fn take_user_handle(&mut self) -> Option<HandleOwner> {
        self.user_handle.take()
    }
}

fn make_thread(
    process: RefPtr<ProcessDispatcher>,
) -> Result<(RefPtr<ThreadDispatcher>, HandleOwner), Status> {
    let (thread_handle, thread_rights) = ThreadDispatcher::create(process, 0, b"userboot")?;
    thread_handle.dispatcher().initialize()?;
    let thread = thread_handle.dispatcher().clone();
    Ok((thread, HandleOwner::make(thread_handle, thread_rights).ok_or(Status::NO_MEMORY)?))
}

fn try_userboot_init(handoff_end: HandoffEnd) -> Result<(), Status> {
    // Create process.
    let status = ProcessDispatcher::create(JobDispatcher::get_root_job(), b"userboot", 0);
    assert!(status.is_ok());
    let (process_handle, process_rights, vmar_handle, vmar_rights) = status.unwrap();

    // Create a root job observer, restarting the system if the root job becomes
    // childless. From now, the life of the system is bound to this first process.
    start_root_job_observer();
    let process = process_handle.dispatcher().clone();
    let process_for_kill = process.clone();
    let mut kill_userboot = defer(move || {
        process_for_kill.kill(ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL);
    });

    // Create thread.
    let (thread, thread_self) = make_thread(process_handle.dispatcher().clone())?;

    // Create bootstrap channel.
    let mut bootstrap_channel = BootstrapChannel::create()?;

    // Handles for the system capability message.
    let mut system_capability_handles = Vector::<Option<HandleOwner>>::new();

    // Pack up VMOs and create userboot loader object.
    let mut userboot = bootstrap_vmos(handoff_end, &mut system_capability_handles)?;

    // Map userboot and obtain vmar_loaded.
    let vmar_loaded = userboot.map(vmar_handle.dispatcher())?;

    // Convert process and root VMAR handles.
    let proc_self = HandleOwner::make(process_handle, process_rights).ok_or(Status::NO_MEMORY)?;
    let vmar_root_self = HandleOwner::make(vmar_handle, vmar_rights).ok_or(Status::NO_MEMORY)?;

    // Create log dispatcher.
    let (log, log_rights) = LogDispatcher::create(0)?;

    let log_for_system_capability =
        HandleOwner::make(KernelHandle::new(log.dispatcher().clone()), log_rights);
    let log_for_process_capability = HandleOwner::make(log, log_rights);

    // Send message 1: the process capability message.
    // This contains essential handles describing the userboot process itself.
    let mut process_capability_handles: [Option<HandleOwner>; 5] = [
        log_for_process_capability,
        proc_self.dup(proc_self.rights()).ok(),
        vmar_root_self.dup(vmar_root_self.rights()).ok(),
        thread_self.dup(thread_self.rights()).ok(),
        vmar_loaded.dup(vmar_loaded.rights()).ok(),
    ];
    bootstrap_channel.send_handles(&mut process_capability_handles)?;

    let mut push_handle = |handle: Option<HandleOwner>| {
        if handle.is_some() {
            let res = system_capability_handles.push_back(handle);
            assert!(res.is_ok());
        }
    };

    // Add process, resource, job, and log handles for the system capability message.
    push_handle(log_for_system_capability);
    push_handle(Some(proc_self));
    push_handle(Some(vmar_root_self));
    push_handle(Some(thread_self));
    push_handle(Some(vmar_loaded));
    push_handle(get_job_handle());
    push_handle(get_resource_handle(ZX_RSRC_KIND_MMIO));
    push_handle(get_resource_handle(ZX_RSRC_KIND_IRQ));
    #[cfg(target_arch = "x86_64")]
    push_handle(get_resource_handle(ZX_RSRC_KIND_IOPORT));
    #[cfg(target_arch = "aarch64")]
    push_handle(get_resource_handle(ZX_RSRC_KIND_SMC));
    push_handle(get_resource_handle(ZX_RSRC_KIND_SYSTEM));

    // Send message 2: the system capability message.
    if let Err(send_status) = bootstrap_channel.send_handles(&mut system_capability_handles) {
        let info = process.get_info();
        kernel_oops!(
            "write on userboot bootstrap channel failed: {}; process retcode {}, flags {:#x}\n",
            send_status.into_raw(),
            info.return_code,
            info.flags
        );
        return Ok(());
    }

    // Start userboot process now that all bootstrap messages are sent.
    userboot.start(&process, thread, bootstrap_channel.take_user_handle())?;

    kill_userboot.cancel();

    TIMELINE_USERBOOT.set(timer_current_mono_ticks().0 as u64);
    INIT_TIME.add(current_mono_time().0 / 1_000_000);
    Ok(())
}

/// Called at the end of the boot process in the main kernel initialization sequence.
pub fn userboot_init(handoff_end: HandoffEnd) {
    if let Err(status) = try_userboot_init(handoff_end) {
        kernel_oops!("userboot_init failed: {}\n", status.into_raw());
    }
}
