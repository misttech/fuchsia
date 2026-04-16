// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of (e)BPF.
//!
//! BPF stands for Berkeley Packet Filter and is an API introduced in BSD that allows filtering
//! network packets by running little programs in the kernel. eBPF stands for extended BFP and
//! is a Linux extension of BPF that allows hooking BPF programs into many different
//! non-networking-related contexts.

pub mod attachments;
pub mod context;
pub mod fs;
pub mod map;
pub mod program;
pub mod syscalls;

use crate::bpf::attachments::EbpfAttachments;
use crate::bpf::map::{BpfMapHandle, BpfMapId, WeakBpfMapHandle};
use crate::bpf::program::{ProgramHandle, ProgramId, WeakProgramHandle};
use starnix_sync::{EbpfStateLock, LockBefore, Locked, OrderedMutex};
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

/// Stores global eBPF state.
#[derive(Default)]
pub struct EbpfState {
    pub attachments: EbpfAttachments,

    programs: OrderedMutex<BTreeMap<ProgramId, WeakProgramHandle>, EbpfStateLock>,
    maps: OrderedMutex<BTreeMap<BpfMapId, WeakBpfMapHandle>, EbpfStateLock>,
}

impl EbpfState {
    fn register_program<L>(&self, locked: &mut Locked<L>, program: &ProgramHandle)
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.programs.lock(locked).insert(program.id(), Arc::downgrade(program));
    }

    fn unregister_program<L>(&self, locked: &mut Locked<L>, id: ProgramId)
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.programs.lock(locked).remove(&id).expect("Missing eBPF program");
    }

    fn get_next_program_id<L>(
        &self,
        locked: &mut Locked<L>,
        start_id: ProgramId,
    ) -> Option<ProgramId>
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.programs
            .lock(locked)
            .range((Bound::Excluded(start_id), Bound::Unbounded))
            .next()
            .map(|(k, _)| *k)
    }

    fn get_program_by_id<L>(&self, locked: &mut Locked<L>, id: ProgramId) -> Option<ProgramHandle>
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.programs.lock(locked).get(&id).map(|p| p.upgrade()).flatten()
    }

    fn register_map<L>(&self, locked: &mut Locked<L>, map: &BpfMapHandle)
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.maps.lock(locked).insert(map.id(), Arc::downgrade(map));
    }

    fn unregister_map<L>(&self, locked: &mut Locked<L>, id: BpfMapId)
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.maps.lock(locked).remove(&id).expect("Missing eBPF map");
    }

    fn get_next_map_id<L>(&self, locked: &mut Locked<L>, start_id: BpfMapId) -> Option<BpfMapId>
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.maps
            .lock(locked)
            .range((Bound::Excluded(start_id), Bound::Unbounded))
            .next()
            .map(|(k, _)| *k)
    }

    fn get_map_by_id<L>(&self, locked: &mut Locked<L>, id: BpfMapId) -> Option<BpfMapHandle>
    where
        L: LockBefore<EbpfStateLock>,
    {
        self.maps.lock(locked).get(&id).map(|p| p.upgrade()).flatten()
    }
}
