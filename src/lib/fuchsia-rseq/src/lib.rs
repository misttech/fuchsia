// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::mem;
use std::ptr::NonNull;
use zx_types::zx_rseq_t;

thread_local! {
    /// The `zx_rseq_t` structure for the current thread, if any.
    ///
    /// This structure is used to store the restartable sequence information for the current thread.
    /// It is registered with the kernel when the thread is created and unregistered when the thread
    /// is destroyed.
    static RSEQ: std::cell::Cell<*mut zx_rseq_t> = Default::default();
}

/// The restartable sequence for the current thread.
///
/// This structure is used to store the restartable sequence information for the current thread.
/// It is registered with the kernel when the thread is created and unregistered when the thread
/// is destroyed.
#[derive(Debug, Clone, Copy)]
pub struct Rseq {
    abi: NonNull<zx_rseq_t>,
}

impl Rseq {
    /// Returns the restartable sequence for the current thread.
    ///
    /// # Panics
    ///
    /// Panics if the thread has not been registered for restartable sequences.
    /// See `rseq_register_thread()`.
    ///
    /// # Safety
    ///
    /// The returned object must not be used after the current thread calls
    /// `rseq_unregister_thread()`.
    pub unsafe fn get() -> Self {
        let abi = NonNull::new(RSEQ.with(|rseq| rseq.get())).expect("thread not registered");
        Self { abi }
    }

    /// Returns a pointer to the `zx_rseq_t` structure.
    ///
    /// The returned pointer is only valid until the current thread calls
    /// `rseq_unregister_thread()`.
    ///
    /// Useful for accessing the `zx_rseq_t` structure from inline assembly.
    pub fn as_ptr(&self) -> *mut zx_rseq_t {
        self.abi.as_ptr()
    }

    /// Reads the current CPU ID.
    ///
    /// The current CPU can change at any time. Useful for preparing for a restartable sequence.
    ///
    /// # Safety
    ///
    /// This method cannot be used after the current thread calls `rseq_unregister_thread()`. That
    /// invariant is required by the safety contract of `get()` as well.
    pub unsafe fn current_cpu(&self) -> u32 {
        let abi = self.as_ptr();
        unsafe {
            let cpu_id_ptr: *mut u32 = std::ptr::addr_of_mut!((*abi).cpu_id);
            // The CPU ID is only written by the kernel on this thread, which means a volatile
            // read is necessary and sufficient.
            std::ptr::read_volatile(cpu_id_ptr)
        }
    }

    /// Activate a critical section.
    ///
    /// The critical section is defined by the `critical_section` parameter. The critical section is
    /// active until the returned `RseqScope` is dropped.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the returned `RseqScope` is dropped before calling
    /// `rseq_unregister_thread()`.
    #[must_use = "The critical section is active only until the returned `RseqScope` is dropped."]
    pub unsafe fn activate(&self, critical_section: RseqCriticalSection) -> RseqScope {
        self.set_critical_section(critical_section);
        RseqScope { rseq: *self }
    }

    fn set_critical_section(&self, critical_section: RseqCriticalSection) {
        let abi = self.as_ptr();
        unsafe {
            let start_ip_ptr: *mut u64 = std::ptr::addr_of_mut!((*abi).start_ip);
            let post_commit_offset_ptr: *mut u64 =
                std::ptr::addr_of_mut!((*abi).post_commit_offset);
            let abort_ip_ptr: *mut u64 = std::ptr::addr_of_mut!((*abi).abort_ip);

            // The kernel reads these values on the current thread, which means a volatile
            // write is necessary and sufficient.

            // We first set the post-commit offset to 0, which prevents the kernel from entering
            // the critical section. Then we set the start and abort IPs, and finally the
            // post-commit offset, which enables the critical section.
            std::ptr::write_volatile(post_commit_offset_ptr, 0);
            std::ptr::write_volatile(start_ip_ptr, critical_section.start_ip);
            std::ptr::write_volatile(abort_ip_ptr, critical_section.abort_ip);
            std::ptr::write_volatile(post_commit_offset_ptr, critical_section.post_commit_offset);
        }
    }
}

/// The bounds of a critical section.
#[derive(Debug, Default, Clone, Copy)]
pub struct RseqCriticalSection {
    /// The address of the start of the critical section.
    start_ip: u64,

    /// The offset from the start of the critical section to the post-commit label.
    post_commit_offset: u64,

    /// The offset from the start of the critical section to the abort label.
    abort_ip: u64,
}

impl RseqCriticalSection {
    /// Creates a new critical section.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the `start_ip`, `post_commit_offset`, and `abort_ip`
    /// parameters correctly define the critical section.
    pub unsafe fn new(start_ip: u64, post_commit_offset: u64, abort_ip: u64) -> Self {
        Self { start_ip, post_commit_offset, abort_ip }
    }
}

/// A RAII guard that clears the critical section bounds when dropped.
pub struct RseqScope {
    rseq: Rseq,
}

impl Drop for RseqScope {
    fn drop(&mut self) {
        self.rseq.set_critical_section(RseqCriticalSection::default());
    }
}

/// Register the current thread for restartable sequences.
///
/// # Panics
///
/// Panics if the thread is already registered.
pub fn rseq_register_thread() -> Result<(), zx::Status> {
    let vmo = zx::Vmo::create(mem::size_of::<zx_rseq_t>() as u64).expect("failed to create VMO");
    let flags = zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE;
    let status = unsafe {
        zx::sys::zx_thread_set_rseq(vmo.raw_handle(), 0, mem::size_of::<zx_rseq_t>() as u64)
    };

    zx::Status::ok(status)?;

    // Currently, we use an entire page per thread for the `zx_rseq_t` structure.
    // This is wasteful, especially for processes that have many threads. In the future, we should
    // either use the VMO that backs thread-local storage, or we should use a single VMO for all
    // threads and use an allocator to manage the space within that VMO.
    let addr = fuchsia_runtime::vmar_root_self().map(
        0,
        &vmo,
        0,
        mem::size_of::<zx_rseq_t>() as usize,
        flags,
    )?;

    let abi = addr as *mut zx_rseq_t;
    RSEQ.with(|rseq| {
        let previous = rseq.replace(abi);
        assert!(previous.is_null());
    });
    Ok(())
}

/// Unregister the current thread from the restartable sequence.
///
/// # Panics
///
/// Panics if the thread is not registered.
pub fn rseq_unregister_thread() -> Result<(), zx::Status> {
    let abi = RSEQ.with(|rseq| rseq.take());
    assert!(!abi.is_null());

    let status = unsafe { zx::sys::zx_thread_set_rseq(0, 0, 0) };
    zx::Status::ok(status)?;

    let addr = abi as usize;
    // SAFETY: this mapping was created by us, no other references to it exist.
    unsafe {
        fuchsia_runtime::vmar_root_self().unmap(addr, mem::size_of::<zx_rseq_t>() as usize)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::arch::global_asm;

    unsafe extern "C" {
        fn rseq_test_critical_section(counters: *mut u64, rseq_ptr: *mut zx_rseq_t);
        static rseq_test_start: u8;
        static rseq_test_end: u8;
    }

    #[cfg(target_arch = "x86_64")]
    global_asm!(
        ".globl rseq_test_critical_section",
        ".globl rseq_test_start",
        ".globl rseq_test_end",
        "rseq_test_critical_section:",
        "rseq_test_start:",
        // Arguments: rdi = counters, rsi = rseq_ptr
        "mov ecx, [rsi]",      // READ cpu_id from rseq_ptr into ecx
        "shl rcx, 3",          // multiply by 8
        "add rcx, rdi",        // address of counter
        "inc qword ptr [rcx]", // increment counter
        "rseq_test_end:",
        "ret",
    );

    #[cfg(target_arch = "aarch64")]
    global_asm!(
        ".globl rseq_test_critical_section",
        ".globl rseq_test_start",
        ".globl rseq_test_end",
        "rseq_test_critical_section:",
        "rseq_test_start:",
        // x0 = counters, x1 = rseq_ptr
        "ldr w2, [x1]",   // read cpu_id
        "lsl x2, x2, #3", // multiply by 8
        "add x2, x0, x2", // address of counter
        "ldr x3, [x2]",
        "add x3, x3, #1",
        "str x3, [x2]",
        "rseq_test_end:",
        "ret",
    );

    #[cfg(target_arch = "riscv64")]
    global_asm!(
        ".globl rseq_test_critical_section",
        ".globl rseq_test_start",
        ".globl rseq_test_end",
        "rseq_test_critical_section:",
        "rseq_test_start:",
        // a0 = counters, a1 = rseq_ptr
        "lw t0, 0(a1)",   // read cpu_id
        "slli t0, t0, 3", // multiply by 8
        "add t0, a0, t0", // address of counter
        "ld t1, 0(t0)",
        "addi t1, t1, 1",
        "sd t1, 0(t0)",
        "rseq_test_end:",
        "ret",
    );

    #[test]
    fn test_current_cpu() {
        rseq_register_thread().expect("register thread failed");
        unsafe {
            let rseq = Rseq::get();
            let cpu_id = rseq.current_cpu();
            assert_ne!(cpu_id, zx_types::ZX_INFO_INVALID_CPU);
        }
        rseq_unregister_thread().expect("unregister thread failed");
    }

    #[test]
    fn test_rseq_per_cpu_counter() {
        rseq_register_thread().expect("register thread failed");

        let rseq = unsafe { Rseq::get() };
        let cpu_count = zx::system_get_num_cpus();
        let mut counters = vec![0u64; cpu_count as usize];

        unsafe {
            let start = &rseq_test_start as *const u8 as u64;
            let end = &rseq_test_end as *const u8 as u64;

            let cs = RseqCriticalSection::new(start, end - start, start);
            let _scope = rseq.activate(cs);

            // Execute the critical section
            rseq_test_critical_section(counters.as_mut_ptr(), rseq.as_ptr());
        }

        let sum = counters.iter().sum::<u64>();
        assert_eq!(sum, 1, "Sum of counters should be 1");

        rseq_unregister_thread().expect("unregister thread failed");
    }
}
