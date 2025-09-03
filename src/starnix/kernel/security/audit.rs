// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::vfs::socket::AuditNetlinkClient;
use arc_swap::ArcSwapWeak;
use bstr::{BString, ByteSlice};
use linux_uapi::{
    AUDIT_FAIL_PANIC, AUDIT_FAIL_PRINTK, AUDIT_FAIL_SILENT, AUDIT_GET, AUDIT_SET,
    AUDIT_STATUS_BACKLOG_LIMIT, AUDIT_STATUS_ENABLED, AUDIT_STATUS_FAILURE, AUDIT_STATUS_LOST,
    AUDIT_STATUS_PID, AUDIT_USER,
};
use starnix_logging::log_warn;
use starnix_sync::Mutex;
use starnix_uapi::errors::Errno;
use starnix_uapi::{audit_status, error, pid_t};
use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
use std::u32;

use crate::task::Kernel;
const DEFAULT_BACKLOG_LIMIT: u32 = 128;

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
    /// The maximum number of audit messages that can be stored by the logger.
    backlog_limit: AtomicU32,
    /// Action to take in case of audit failure.
    fail_action: AtomicU8,
    /// The PID of the process registered as the audit daemon.
    audit_sink_pid: AtomicI32,
    /// Socket to which the logger writes audit messages.
    audit_sink: ArcSwapWeak<AuditNetlinkClient>,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            // TODO: https://fxbug.dev/438671380 - should be disabled by default.
            enabled: AtomicBool::new(true),
            backlog_limit: AtomicU32::new(DEFAULT_BACKLOG_LIMIT),
            fail_action: AtomicU8::new(AUDIT_FAIL_PRINTK as u8),
            audit_sink_pid: Default::default(),
            audit_sink: Default::default(),
        }
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
    /// Audit message deque containing (audit type, audit string) up to `backlog_limit` messages.
    /// TODO: https://fxbug.dev/438677236 - confirm single queue behaviour is valid.
    audit_queue: Mutex<VecDeque<AuditMessage>>,
}

impl AuditLogger {
    pub fn new(kernel: &Kernel) -> Self {
        Self {
            configuration: AuditConfig::new(&kernel.cmdline),
            lost_audit_messages: Default::default(),
            audit_queue: Default::default(),
        }
    }

    /// Audit logging function that adds an audit message to the queue.
    ///
    /// The `audit_formatter` function is called only if the auditing is enabled.
    pub fn audit_log<M: Display, T: FnOnce() -> M>(&self, audit_type: u16, audit_formatter: T) {
        if !self.configuration.enabled.load(Ordering::Acquire) {
            return;
        }
        let audit_message = audit_formatter();
        let mut queue = self.audit_queue.lock();

        // TODO: https://fxbug.dev/440090442 - implement backlog waiting.
        if !self.check_backlog(queue.len() as u32) {
            queue
                .push_back(AuditMessage { audit_type, message: format!("{audit_message}").into() });
            self.configuration.audit_sink.load().upgrade().inspect(|sink| sink.notify()).or_else(
                || {
                    log_warn!("{audit_message}");
                    None
                },
            );
        }
    }

    /// Called by the `NetlinkAuditClient` to pull the next audit log from the backlog.
    pub fn read_audit_log(&self, pid: pid_t) -> Option<AuditMessage> {
        if self.configuration.audit_sink_pid.load(Ordering::Acquire) != pid {
            return None;
        }
        self.audit_queue.lock().pop_front()
    }

    /// Function to detach the `AuditNetlinkClient` from the `AuditLogger`.
    pub fn detach_client(&self) {
        self.configuration.audit_sink_pid.store(0, Ordering::Release);
        self.configuration.audit_sink.store(Weak::new());
    }

    /// Applies the specified changes to the audit logger settings.
    ///
    /// If the `AUDIT_STATUS_PID` bit is set in `status.mask`, the `client` must be valid.
    pub fn set_status(
        &self,
        status: audit_status,
        client: Weak<AuditNetlinkClient>,
    ) -> Result<(), Errno> {
        if status.mask & AUDIT_STATUS_ENABLED != 0 {
            self.configuration.enabled.store(status.enabled != 0, Ordering::Release);
        }
        if status.mask & AUDIT_STATUS_BACKLOG_LIMIT != 0 {
            self.configuration.backlog_limit.store(status.backlog_limit, Ordering::Release);
        }
        if status.mask & AUDIT_STATUS_FAILURE != 0 {
            self.configuration.fail_action.store(status.failure as u8, Ordering::Release);
        }
        if status.mask & AUDIT_STATUS_LOST != 0 {
            self.lost_audit_messages.store(0, Ordering::Release);
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
            self.configuration.audit_sink.store(client);
        }
        Ok(())
    }

    /// Retrieve the `AuditConfig` as `audit_status` struct.
    pub fn get_status(&self) -> audit_status {
        audit_status {
            mask: Default::default(),
            enabled: self.configuration.enabled.load(Ordering::Acquire) as u32,
            failure: self.configuration.fail_action.load(Ordering::Acquire) as u32,
            pid: self.configuration.audit_sink_pid.load(Ordering::Acquire) as u32,
            rate_limit: u32::MAX,
            backlog_limit: self.configuration.backlog_limit.load(Ordering::Acquire),
            lost: self.lost_audit_messages.load(Ordering::Acquire),
            backlog: self.audit_queue.lock().len() as u32,
            __bindgen_anon_1: Default::default(),
            backlog_wait_time: Default::default(),
            backlog_wait_time_actual: Default::default(),
        }
    }

    /// Retrieve the number of audit messages in the backlog.
    pub fn get_backlog_count(&self, pid: pid_t) -> usize {
        if self.configuration.audit_sink_pid.load(Ordering::Acquire) != pid {
            return 0;
        }
        self.audit_queue.lock().len()
    }

    /// Function to check the backlog size against the backlog limit.
    /// If the limit is set to 0, ignore the check.
    ///
    /// Return true if the limit is reached, false otherwise.
    fn check_backlog(&self, backlog_size: u32) -> bool {
        let limit = self.configuration.backlog_limit.load(Ordering::Acquire);
        if limit != 0 && backlog_size >= limit {
            let lost = self.lost_audit_messages.fetch_add(1, Ordering::Release) + 1;
            log_warn!("audit_lost={lost} backlog_limit={limit}");
            // If the backlog is full, use failure-to-log action.
            match self.configuration.fail_action.load(Ordering::Acquire) as u32 {
                AUDIT_FAIL_PANIC => panic!("backlog limit exceeded"),
                AUDIT_FAIL_PRINTK => log_warn!("backlog limit exceeded"),
                AUDIT_FAIL_SILENT | _ => (),
            }
            return true;
        }
        false
    }
}

/// Audit message structure.
pub struct AuditMessage {
    /// The type of the audit message (e.g., AUDIT_AVC).
    pub audit_type: u16,
    /// The message to be audit-logged.
    pub message: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use crate::security::AuditLogger;
    use crate::testing::spawn_kernel_and_run;
    use linux_uapi::{AUDIT_STATUS_PID, audit_status};
    use std::sync::Weak;

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
            let _ = afw.set_status(status, Weak::new());

            let mut recv_status = afw.get_status();
            assert_eq!(status.pid, recv_status.pid);
            assert_ne!(status.rate_limit, recv_status.rate_limit);

            afw.detach_client();
            recv_status = afw.get_status();
            assert_eq!(0, recv_status.pid);
        })
    }
}
