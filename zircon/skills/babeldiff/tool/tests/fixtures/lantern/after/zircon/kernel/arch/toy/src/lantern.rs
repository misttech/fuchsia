// A synthetic lantern driver for babeldiff's tests. It is not real kernel
// code; each function exists to plant one kind of conversion mistake.

//! Ported from `zircon/kernel/arch/toy/lantern.cc`.

use core::arch::asm;

fn read_brightness() -> u64 {
    let value: u64;
    // SAFETY: Reading the brightness register has no side effects.
    unsafe {
        asm!("mov {}, dr6", out(reg) value, options(nomem, nostack));
    }
    value | LANTERN_BRIGHT_MASK
}

pub fn lantern_save(state: &mut LanternState) {
    state.brightness = read_brightness();
    state.mode = lantern_mode();
}

pub fn lantern_copy_from_frame(out: &mut LanternState, inp: &Frame) {
    out.wick0 = inp.wick0;
    out.wick1 = inp.wick1;
    out.wick2 = inp.wick2;
    out.flags = inp.flags & !LANTERN_FLAGS_RESUME;
}

pub fn lantern_copy_from_syscall(out: &mut LanternState, inp: &SyscallFrame) {
    out.wick0 = inp.wick0;
    out.wick1 = inp.wick1;
    out.wick2 = inp.wick2;
    out.flags = inp.flags;
}

pub fn lantern_get_colors(lantern: &Lantern, out: &mut LanternColors) -> zx_status_t {
    // Red first, then green and blue.
    out.red.copy_from_slice(&lantern.red[..COLOR_SIZE]);
    out.green.copy_from_slice(&lantern.green[..COLOR_SIZE]);
    out.blue.copy_from_slice(&lantern.blue[..COLOR_SIZE]);
    ZX_OK
}

/// Reads the lantern's state (`lantern.cc:52-70`).
///
/// # Safety
///
/// `lantern` and `out` must be null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lantern_get_state(lantern: *mut Lantern, out: *mut LanternState) -> zx_status_t {
    if lantern.is_null() || out.is_null() {
        return ZX_ERR_INVALID_ARGS;
    }
    let f = |lantern: *mut Lantern| -> zx_status_t {
        // SAFETY: `lantern` is valid and its lock is held.
        let lit = unsafe { lantern::is_lit(lantern) };
        debug_assert!(lit);
        // SAFETY: `lantern` is valid and its lock is held.
        let l = unsafe { &*lantern };
        // Nothing to report until the lantern has been lit.
        if l.frame.is_null() {
            return ZX_ERR_BAD_STATE;
        }
        // SAFETY: `out` is non-null and valid.
        let out = unsafe { &mut *out };
        if l.source == LanternSource::Frame {
            // SAFETY: `frame` is non-null.
            lantern_copy_from_frame(out, unsafe { &*l.frame });
        } else if l.source == LanternSource::Syscall {
            // SAFETY: A syscall frame is set whenever the source says so.
            lantern_copy_from_syscall(out, unsafe { &*l.syscall_frame });
        } else {
            return ZX_ERR_NOT_SUPPORTED;
        }
        out.level = l.level;
        ZX_OK
    };
    // SAFETY: `lantern` is non-null and valid.
    unsafe { lantern::with_chain_lock(lantern, f) }
}

// Statically initialized, like the C++ globals were.
static SAVED_LEVEL: u64 = 0;

// Starts out lit, unlike the C++ `kDark` default
// mode.
static SAVE_TOUCHES_FRAME: bool = false;
