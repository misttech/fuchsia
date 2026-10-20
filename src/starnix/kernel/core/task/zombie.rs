// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::signals::syscalls::WaitingOptions;
use crate::signals::{SignalDetail, SignalInfo};
use crate::task::{
    ExitStatus, Pid, PidTableGuard, ProcessSelector, Task, ThreadGroup, ThreadGroupStateRef,
};
use starnix_logging::log_warn;
use starnix_types::ownership::{OwnedRef, Releasable};
use starnix_types::stats::TaskTimeStats;
use starnix_uapi::signals::{SIGCHLD, Signal};
use starnix_uapi::uid_t;
use std::sync::{Arc, Weak};

#[derive(Debug)]
pub struct ZombieProcess {
    pub task: Arc<Task>,
    pub exit_signal: Option<Signal>,
    pub state: ZombieState,

    /// Whether dropping this ZombieProcess should imply removing the pid from
    /// the PidTable
    pub is_canonical: bool,
}

impl PartialEq for ZombieProcess {
    fn eq(&self, other: &Self) -> bool {
        // We assume only one set of ZombieProcess data per process, so this should cover it.
        self.task.pid == other.task.pid && self.is_canonical == other.is_canonical
    }
}

impl Eq for ZombieProcess {}

impl PartialOrd for ZombieProcess {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ZombieProcess {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.task.pid, self.is_canonical).cmp(&(&other.task.pid, other.is_canonical))
    }
}

impl ZombieProcess {
    pub fn new(
        task: Arc<Task>,
        thread_group: ThreadGroupStateRef<'_>,
        exit_status: ExitStatus,
        exit_signal: Option<Signal>,
    ) -> OwnedRef<Self> {
        let time_stats = thread_group.base.time_stats() + thread_group.children_time_stats;
        OwnedRef::new(ZombieProcess {
            task,
            state: ZombieState { exit_status, time_stats },
            exit_signal,
            is_canonical: true,
        })
    }

    pub fn to_wait_result(&self) -> WaitResult {
        WaitResult {
            pid: self.task.pid.clone(),
            uid: self.task.real_creds().uid,
            zombie_state: self.state.clone(),
            exit_signal: self.exit_signal,
        }
    }

    pub fn as_artificial(&self) -> Self {
        ZombieProcess {
            task: self.task.clone(),
            state: self.state.clone(),
            exit_signal: self.exit_signal,
            is_canonical: false,
        }
    }

    pub fn matches_selector(&self, selector: &ProcessSelector) -> bool {
        match selector {
            ProcessSelector::Any => true,
            ProcessSelector::Pid(pid) => &self.task.pid == pid,
            ProcessSelector::Pgid(pgid) => &self.task.thread_group().pgid() == pgid,
        }
    }

    pub fn matches_selector_and_waiting_option(
        &self,
        selector: &ProcessSelector,
        options: &WaitingOptions,
    ) -> bool {
        if !self.matches_selector(selector) {
            return false;
        }

        if options.wait_for_all {
            true
        } else {
            // A "clone" zombie is one which has delivered no signal, or a
            // signal other than SIGCHLD to its parent upon termination.
            options.wait_for_clone == (self.exit_signal != Some(SIGCHLD))
        }
    }
}

/// Trait for releasing a zombie process from the PID table.
///
/// This trait erases the lifetime parameter of [`PidTableGuard`] so that [`ZombieProcess`] can
/// implement [`Releasable`] without tying the mutable reference lifetime to the guard's lifetime
/// parameter, preserving variance and allowing reborrowing in loops and across sequential calls.
pub trait ZombieReleaser {
    fn remove_zombie(&mut self, pid: &Pid);
}

impl<'a> ZombieReleaser for PidTableGuard<'a> {
    fn remove_zombie(&mut self, pid: &Pid) {
        self.remove_zombie(pid);
    }
}

impl Releasable for ZombieProcess {
    type Context<'a> = &'a mut dyn ZombieReleaser;

    fn release<'a>(self, pids: &'a mut dyn ZombieReleaser) {
        if self.is_canonical {
            pids.remove_zombie(&self.task.pid);
        }
    }
}

/// A zombie process that is pending notification.
///
/// # Thread Safety
///
/// Notifications are generally produced in contexts in which a [`ThreadGroup`] state lock is held.
/// Any such lock must be released before notifications are delivered. The notification's
/// recipient thread group may be:
/// - The originating thread group, in which case delivery while locked would self-deadlock.
/// - One of this thread group's ancestors, in which case delivery while locked would invert the
///   parent-child ordering of [`ThreadGroup`] locks.
///
/// The [`PidTable`] lock must be held continuously between [`ZombieNotification`] production and
/// delivery to protect against concurrent exit races. Delivery requires releasing [`ThreadGroup`]
/// state locks. If the recipient thread group exits before the notification is delivered, subreaper
/// identification becomes impossible and the zombie must be reaped without notifying observers.
/// Holding the [`PidTable`] lock throughout notification ensures the recipient cannot concurrently
/// exit.
#[must_use = "Notifications must be explicitly delivered or discarded"]
pub struct ZombieNotification {
    /// The recipient [`ThreadGroup`], which is generally the zombie's parent.
    pub recipient: Weak<ThreadGroup>,

    /// The zombie process to notify the parent of.
    pub zombie: OwnedRef<ZombieProcess>,
}

impl ZombieNotification {
    pub fn new(recipient: Weak<ThreadGroup>, zombie: OwnedRef<ZombieProcess>) -> Self {
        Self { recipient, zombie }
    }

    /// Delivers the zombie notification to the parent.
    ///
    /// # Thread Safety
    ///
    /// Acquires [`ThreadGroup`] state locks.
    pub fn deliver(self, pids: &mut PidTableGuard<'_>) {
        if let Some(parent) = self.recipient.upgrade() {
            parent.do_zombie_notifications(self.zombie, pids);
        } else {
            log_warn!("Zombie {} reaped silently", self.zombie.task.get_pid());
            self.zombie.release(pids);
        }
    }

    /// Discards the zombie notification without delivering it.
    ///
    /// If the [`ZombieProcess`] has no other owners, it will be reaped.
    pub fn discard(self, pids: &mut PidTableGuard<'_>) {
        self.zombie.release(pids);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitResult {
    pub pid: Pid,
    pub uid: uid_t,
    pub zombie_state: ZombieState,
    pub exit_signal: Option<Signal>,
}

impl WaitResult {
    // According to wait(2) man page, SignalInfo.signal needs to always be set to SIGCHLD
    pub fn as_signal_info(&self) -> SignalInfo {
        SignalInfo::with_detail(
            SIGCHLD,
            self.zombie_state.exit_status.signal_info_code(),
            SignalDetail::SIGCHLD {
                pid: self.pid.clone(),
                uid: self.uid,
                status: self.zombie_state.exit_status.signal_info_status(),
            },
        )
    }
}

/// State of a task or thread group which has exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZombieState {
    pub exit_status: ExitStatus,
    pub time_stats: TaskTimeStats,
}
