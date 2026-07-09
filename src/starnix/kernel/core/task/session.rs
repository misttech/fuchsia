// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_sync::{LockDepRwLock, SessionMutableStateLock};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use crate::device::terminal::Terminal;
use crate::task::ProcessGroup;
use starnix_uapi::pid_t;
use starnix_uapi::signals::{SIGCONT, SIGHUP};
use std::ops::{Deref, DerefMut};

#[derive(Debug)]
pub struct SessionMutableState {
    /// The process groups in the session
    ///
    /// The references to ProcessGroup is weak to prevent cycles as ProcessGroup have a Arc reference to their
    /// session.
    /// It is still expected that these weak references are always valid, as process groups must unregister
    /// themselves before they are deleted.
    process_groups: BTreeMap<pid_t, Weak<ProcessGroup>>,

    /// The leader of the foreground process group. This is necessary because the leader must
    /// be returned even if the process group has already been deleted.
    foreground_process_group: pid_t,

    /// The controlling terminal of the session.
    pub controlling_terminal: Option<ControllingTerminal>,
}

/// A session is a collection of `ProcessGroup` objects that are related to each other. Each
/// session has a session ID (`sid`), which is a unique identifier for the session.
///
/// The session leader is the first `ProcessGroup` in a session. It is responsible for managing the
/// session, including sending signals to all processes in the session and controlling the
/// foreground and background process groups.
///
/// When a `ProcessGroup` is created, it is automatically added to the session of its parent.
/// See `setsid(2)` for information about creating sessions.
///
/// A session can be destroyed when the session leader exits or when all process groups in the
/// session are destroyed.
#[derive(Debug)]
pub struct Session {
    /// The leader of the session
    pub leader: pid_t,

    /// The mutable state of the Session.
    pub mutable_state: LockDepRwLock<SessionMutableState, SessionMutableStateLock>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.leader == other.leader
    }
}

impl Session {
    pub fn new(leader: pid_t) -> Arc<Session> {
        Arc::new(Session {
            leader,
            mutable_state: SessionMutableState {
                process_groups: BTreeMap::new(),
                foreground_process_group: leader,
                controlling_terminal: None,
            }
            .into(),
        })
    }

    /// Disassociates the controlling terminal from the session.
    pub fn disassociate_controlling_terminal(&self) {
        loop {
            // THREAD SAFETY: The controlling terminal must be extracted from the Session state
            // lock. Respect Terminal => Session lock ordering by dropping the Session lock before
            // acquiring the Terminal lock. The controlling terminal may change while reacquiring
            // locks.
            let Some(controlling_terminal) = self.read().controlling_terminal.clone() else {
                return;
            };
            let mut terminal_state = controlling_terminal.terminal.write();
            let mut state = self.write();

            // THREAD SAFETY: Check whether the controlling terminal changed while the Session lock
            // was dropped.
            if !state.controlling_terminal.as_ref().map_or(false, |current_ct| {
                current_ct.matches(&controlling_terminal.terminal, controlling_terminal.is_main)
            }) {
                // Drop the lock for the old terminal and try again.
                continue;
            }

            state.controlling_terminal = None;
            terminal_state.controller = None;

            // THREAD SAFETY: Respect ThreadGroup => Terminal => Session lock ordering by dropping
            // the Terminal and Session locks before signaling.
            let process_group = state.get_foreground_process_group();
            drop(state);
            drop(terminal_state);
            if let Some(pg) = process_group {
                pg.send_signals(&[SIGHUP, SIGCONT]);
            }
            return;
        }
    }

    pub fn read(&self) -> impl Deref<Target = SessionMutableState> {
        self.mutable_state.read()
    }

    pub fn write(&self) -> impl DerefMut<Target = SessionMutableState> {
        self.mutable_state.write()
    }
}

impl SessionMutableState {
    /// Removes the process group from the session. Returns whether the session is empty.
    pub fn remove(&mut self, pid: pid_t) {
        self.process_groups.remove(&pid);
    }

    pub fn insert(&mut self, process_group: &Arc<ProcessGroup>) {
        self.process_groups.insert(process_group.leader, Arc::downgrade(process_group));
    }

    pub fn get_foreground_process_group_leader(&self) -> pid_t {
        self.foreground_process_group
    }

    pub fn get_foreground_process_group(&self) -> Option<Arc<ProcessGroup>> {
        self.process_groups.get(&self.foreground_process_group).and_then(Weak::upgrade)
    }

    pub fn set_foreground_process_group(&mut self, process_group: &Arc<ProcessGroup>) {
        self.foreground_process_group = process_group.leader;
    }
}

/// The controlling terminal of a session.
#[derive(Clone, Debug)]
pub struct ControllingTerminal {
    /// The controlling terminal.
    pub terminal: Arc<Terminal>,
    /// Whether the session is associated to the main or replica side of the terminal.
    pub is_main: bool,
}

impl ControllingTerminal {
    pub fn new(terminal: &Terminal, is_main: bool) -> Self {
        Self { terminal: terminal.to_owned(), is_main }
    }

    pub fn matches(&self, terminal: &Terminal, is_main: bool) -> bool {
        std::ptr::eq(terminal, Arc::as_ptr(&self.terminal)) && is_main == self.is_main
    }
}

/// Represents the disassociation of a session's controlling terminal when the session
/// leader exits.
///
/// This struct wraps an optional session and ensures that `disassociate_controlling_terminal`
/// is explicitly called by the caller, which must be done without holding any
/// ThreadGroup's write lock.
#[must_use = "The controlling terminal must be disassociated when the session leader exits."]
pub struct SessionDisassociation {
    session: Option<Arc<Session>>,
}

impl SessionDisassociation {
    pub(crate) fn new(session: Option<Arc<Session>>) -> Self {
        Self { session }
    }

    /// Disassociates the controlling terminal from the session.
    ///
    /// If the exiting thread group is the session leader, the controlling terminal must be
    /// disassociated. This must be called after dropping the ThreadGroup write lock to
    /// prevent a lock order violation.
    ///
    /// Calling it after the thread group has left the process group also ensures that
    /// the exiting thread group is no longer in the process group when attempting to send
    /// SIGHUP/SIGCONT to the foreground process group, avoiding a self-deadlock where the
    /// exiting thread group attempts to write-lock itself.
    pub fn disassociate_controlling_terminal(self) {
        if let Some(session) = self.session {
            session.disassociate_controlling_terminal();
        }
    }
}
