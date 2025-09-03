// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bstr::{BString, ByteSlice};
use linux_uapi::{AUDIT_GET, AUDIT_SET, AUDIT_STATUS_ENABLED, AUDIT_STATUS_PID, AUDIT_USER};
use starnix_logging::log_warn;
use starnix_uapi::errors::Errno;
use starnix_uapi::{audit_status, error, pid_t};
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::u32;

use crate::task::Kernel;

/// Supported requests that manipulate the `AuditLogger`
#[repr(u32)]
pub enum AuditRequest {
    AuditGet = AUDIT_GET,
    AuditSet = AUDIT_SET,
    AuditUser = AUDIT_USER,
}

impl TryFrom<u32> for AuditRequest {
    type Error = Errno;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            AUDIT_GET => Ok(Self::AuditGet),
            AUDIT_SET => Ok(Self::AuditSet),
            AUDIT_USER => Ok(Self::AuditUser),
            _ => error!(ENOTSUP),
        }
    }
}

/// Audit status structure defining the behaviour of the logger.
struct AuditConfig {
    enabled: AtomicBool,
    /// The PID of the process registered as the audit daemon.
    audit_sink_pid: AtomicI32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        // TODO: https://fxbug.dev/438671380 - should be disabled by default.
        Self { enabled: AtomicBool::new(true), audit_sink_pid: Default::default() }
    }
}

impl AuditConfig {
    pub fn new(cmdline: &BString) -> Self {
        let config = Self::default();
        // The logger may be disabled by the kernel command line.
        // TODO: https://fxbug.dev/440087162 - apply all the kernel cmdline options properly.
        if cmdline.contains_str("audit=0") {
            config.enabled.store(false, Ordering::Release);
        }
        config
    }
}

/// Audit logging structure.
pub struct AuditLogger {
    /// Audit status structure.
    configuration: AuditConfig,
    /// The number of audit messages lost due to writing errors.
    lost_audit_messages: AtomicU32,
}

impl AuditLogger {
    pub fn new(kernel: &Kernel) -> Self {
        Self {
            configuration: AuditConfig::new(&kernel.cmdline),
            lost_audit_messages: Default::default(),
        }
    }

    /// Audit logging function that adds an audit message.
    ///
    /// The `audit_formatter` function is called only if the auditing is enabled.
    pub fn audit_log<M: Display, T: FnOnce() -> M>(&self, audit_formatter: T) {
        if !self.configuration.enabled.load(Ordering::Acquire) {
            return;
        }
        let audit_message = audit_formatter();
        log_warn!("{audit_message}");
    }

    /// Function to detach the `AuditNetlinkClient` from the `AuditLogger`.
    pub fn detach_client(&self) {
        self.configuration.audit_sink_pid.store(0, Ordering::Release);
    }

    /// Set different attributes of the `AuditConfig`.
    pub fn set_status(&self, status: audit_status) -> Result<(), Errno> {
        if status.mask & AUDIT_STATUS_ENABLED != 0 {
            self.configuration.enabled.store(status.enabled != 0, Ordering::Release);
        }
        if status.mask & AUDIT_STATUS_PID != 0 {
            if let Err(_) = self.configuration.audit_sink_pid.compare_exchange(
                0,
                status.pid as pid_t,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                return error!(EINVAL);
            }
        }
        Ok(())
    }

    /// Retrieve the `AuditConfig` as `audit_status` struct.
    pub fn get_status(&self) -> audit_status {
        audit_status {
            mask: Default::default(),
            enabled: self.configuration.enabled.load(Ordering::Acquire) as u32,
            failure: 0,
            pid: self.configuration.audit_sink_pid.load(Ordering::Acquire) as u32,
            rate_limit: u32::MAX,
            backlog_limit: u32::MAX,
            lost: self.lost_audit_messages.load(Ordering::Acquire),
            backlog: 0,
            __bindgen_anon_1: Default::default(),
            backlog_wait_time: Default::default(),
            backlog_wait_time_actual: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::security::AuditLogger;
    use crate::testing::spawn_kernel_and_run;
    use linux_uapi::{AUDIT_STATUS_PID, audit_status};

    #[fuchsia::test]
    async fn test_audit_status_get_and_set() {
        spawn_kernel_and_run(|_locked, current_task| {
            let afw = AuditLogger::new(current_task.kernel());

            let status = audit_status {
                mask: AUDIT_STATUS_PID,
                enabled: 0,
                failure: 0,
                pid: 100,
                rate_limit: 100,
                backlog_limit: 100,
                lost: 100,
                backlog: 0,
                __bindgen_anon_1: Default::default(),
                backlog_wait_time: 0,
                backlog_wait_time_actual: 0,
            };
            let _ = afw.set_status(status);

            let mut recv_status = afw.get_status();
            assert_eq!(status.pid, recv_status.pid);
            assert_ne!(status.rate_limit, recv_status.rate_limit);

            afw.detach_client();
            recv_status = afw.get_status();
            assert_eq!(0, recv_status.pid);
        })
    }
}
