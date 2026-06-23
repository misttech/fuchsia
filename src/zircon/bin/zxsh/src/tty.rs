// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_pty as fpty;
use std::os::fd::AsFd;
use zx::Task;

/// Represents the state of POSIX signals in the shell.
///
/// Note: These are POSIX-like software signals (e.g. SIGINT) managed by the shell,
/// not Zircon signals (which are kernel-level object state flags like `PROCESS_TERMINATED`).
///
/// Tracks received SIGINT and other pending signals.
#[derive(Clone, Copy)]
pub struct ShellSignalState {
    /// True if SIGINT has been received (e.g., Ctrl+C).
    pub sigint_received: bool,
    pending_signals: u32,
}

impl ShellSignalState {
    /// Creates a new empty `ShellSignalState`.
    pub fn new() -> Self {
        Self { sigint_received: false, pending_signals: 0 }
    }

    /// Sets the specified signal as pending.
    pub fn set_pending(&mut self, sig: i32) {
        if sig > 0 && sig < 32 {
            self.pending_signals |= 1 << sig;
        }
    }

    /// Clears the specified pending signal.
    pub fn clear_pending(&mut self, sig: i32) {
        if sig > 0 && sig < 32 {
            self.pending_signals &= !(1 << sig);
        }
    }

    /// Takes all pending signals, clearing them from the state.
    pub fn take_pending(&mut self) -> u32 {
        let pending = self.pending_signals;
        self.pending_signals = 0;
        pending
    }
}

/// Controls a PTY device.
pub struct PtyControl {
    /// The synchronous proxy to the PTY device.
    pub proxy: fpty::DeviceSynchronousProxy,
    /// The event pair associated with the PTY device for signaling.
    pub event: zx::EventPair,
}

/// Attempts to get PTY control from a file descriptor.
///
/// Returns `None` if the file descriptor is not a PTY or if cloning the channel fails.
pub fn get_pty_control(fd: &impl AsFd) -> Option<PtyControl> {
    let channel = fdio::clone_channel(fd).ok()?;
    let proxy = fpty::DeviceSynchronousProxy::new(channel);
    let info = proxy.describe(zx::MonotonicInstant::INFINITE).ok()?;
    let event = info.event?;
    Some(PtyControl { proxy, event })
}

fn kill_and_wait(
    proc: &zx::Process,
    signal_state: &mut ShellSignalState,
) -> Result<zx::Signals, zx::Status> {
    signal_state.set_pending(libc::SIGINT);
    let _ = proc.kill();
    proc.wait_one(zx::Signals::PROCESS_TERMINATED, zx::MonotonicInstant::INFINITE).to_result()
}

/// Waits for a process to terminate, while allowing interruption via PTY events (e.g., Ctrl+C).
///
/// If `pty_control` is provided, it monitors PTY events for interrupts.
/// If SIGINT is received or was already received, it kills the process and returns.
pub fn wait_for_process_with_interrupt(
    proc: &zx::Process,
    pty_control: Option<&PtyControl>,
    signal_state: &mut ShellSignalState,
) -> Result<zx::Signals, zx::Status> {
    if signal_state.sigint_received {
        return kill_and_wait(proc, signal_state);
    }

    if let Some(pty) = pty_control {
        let mut items = [
            proc.wait_item(zx::Signals::PROCESS_TERMINATED),
            pty.event.wait_item(zx::Signals::USER_1 | zx::Signals::OBJECT_PEER_CLOSED),
        ];

        loop {
            if signal_state.sigint_received {
                return kill_and_wait(proc, signal_state);
            }

            match zx::object_wait_many(&mut items, zx::MonotonicInstant::INFINITE) {
                Ok(_) => {
                    if items[0].pending().contains(zx::Signals::PROCESS_TERMINATED) {
                        return Ok(items[0].pending());
                    }
                    if items[1].pending().contains(zx::Signals::USER_1) {
                        match pty.proxy.read_events(zx::MonotonicInstant::INFINITE) {
                            Ok((status, events)) => {
                                if status == zx::Status::OK.into_raw() {
                                    if (events & fpty::EVENT_INTERRUPT) != 0 {
                                        signal_state.sigint_received = true;
                                        return kill_and_wait(proc, signal_state);
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    if items[1].pending().contains(zx::Signals::OBJECT_PEER_CLOSED) {
                        proc.wait_one(
                            zx::Signals::PROCESS_TERMINATED,
                            zx::MonotonicInstant::INFINITE,
                        )
                        .to_result()?;
                        return Ok(zx::Signals::PROCESS_TERMINATED);
                    }
                }
                Err(status) => return Err(status),
            }
        }
    } else {
        proc.wait_one(zx::Signals::PROCESS_TERMINATED, zx::MonotonicInstant::INFINITE).to_result()
    }
}
