// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kprint::kprintln;
use zx_types::{
    ZX_OBJ_TYPE_CHANNEL, ZX_OBJ_TYPE_EVENTPAIR, ZX_OBJ_TYPE_FIFO, ZX_OBJ_TYPE_IOB,
    ZX_OBJ_TYPE_SOCKET,
};

use super::diagnostics_ffi::{
    dump_all_peer_dispatchers, dump_all_vm_objects, dump_handles_for_koid, dump_job_list,
    dump_job_policies, dump_orphaned_mapped_vmos, dump_orphaned_slices,
    dump_peer_dispatchers_by_koid, dump_process_address_space, dump_process_handles,
    dump_process_list, dump_process_memory_usage, dump_process_vm_objects, dump_vm_object_cow_tree,
    handle_diagnostics_dump_table_info, handle_diagnostics_outstanding_handles, kill_process,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::thread;
use crate::platform_rs::timer::DurationMono;
use crate::vm::vm_aspace::VmAspace;

const MB: usize = 1024 * 1024;

static MWD_LIMIT_BYTES: AtomicUsize = AtomicUsize::new(32 * MB);
static MWD_RUNNING: AtomicBool = AtomicBool::new(false);

static HWD_LIMIT: AtomicUsize = AtomicUsize::new(1024);
static HWD_RUNNING: AtomicBool = AtomicBool::new(false);

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
fn dump_address_space(arg: &CmdArgs) {
    if "kernel".starts_with(arg.as_str()) {
        // The arg is a prefix of "kernel".
        VmAspace::kernel_aspace().dump(true);
    } else {
        dump_process_address_space(arg.arg_uint);
    }
}

fn dump_handle_table() {
    kprintln!("outstanding handles: {}", handle_diagnostics_outstanding_handles());
    handle_diagnostics_dump_table_info();
}

extern "C" fn hwd_thread(_arg: *mut c_void) -> i32 {
    static PREVIOUS_HANDLE_COUNT: AtomicUsize = AtomicUsize::new(0);

    loop {
        let handle_count = handle_diagnostics_outstanding_handles();
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

extern "C" fn mwd_thread(_arg: *mut c_void) -> i32 {
    loop {
        let _ = thread::sleep_relative(DurationMono::from_seconds(1));
        dump_process_memory_usage(c"MemoryHog! ", MWD_LIMIT_BYTES.load(Ordering::Relaxed));
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
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

static_command!(
    CMD_ZX,
    c"zx".as_ptr(),
    c"kernel object diagnostics".as_ptr(),
    cmd_diagnostics,
    CMD_AVAIL_NORMAL
);
