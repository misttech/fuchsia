// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use ebpf_api::{CurrentTaskContext, MapValueRef, MapsContext};
use starnix_uapi::{gid_t, pid_t, uid_t};

pub struct EbpfRunContextImpl<'a> {
    current_task: &'a CurrentTask,

    map_refs: Vec<MapValueRef<'a>>,
}

impl<'a> EbpfRunContextImpl<'a> {
    pub fn new(current_task: &'a CurrentTask) -> Self {
        Self { current_task, map_refs: vec![] }
    }
}

impl<'a> MapsContext<'a> for EbpfRunContextImpl<'a> {
    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>) {
        self.map_refs.push(map_ref)
    }
}

impl<'a> CurrentTaskContext for EbpfRunContextImpl<'a> {
    fn get_uid_gid(&self) -> (uid_t, gid_t) {
        let creds = self.current_task.current_creds();
        (creds.uid, creds.gid)
    }

    fn get_tid_tgid(&self) -> (pid_t, pid_t) {
        let task = &self.current_task.task;
        (task.get_tid(), task.get_pid())
    }
}
