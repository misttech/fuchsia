// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::memory::MemoryObject;
use crate::security;
use crate::task::{CurrentTask, CurrentTaskAndLocked, Kernel, register_delayed_release};
use ebpf::MapSchema;
use ebpf_api::{Map, MapError, PinnedMap};
use starnix_lifecycle::{ObjectReleaser, ReleaserAction};
use starnix_sync::{
    EbpfMapStateLevel, EbpfStateLock, LockBefore, Locked, MutexGuard, OrderedMutex,
};
use starnix_types::ownership::{Releasable, ReleaseGuard};
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Weak};

pub type BpfMapId = u32;

/// Counter for map identifiers.
static MAP_IDS: AtomicU32 = AtomicU32::new(1);
fn new_map_id() -> BpfMapId {
    MAP_IDS.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn map_error_to_errno(e: MapError) -> Errno {
    match e {
        MapError::InvalidParam => errno!(EINVAL),
        MapError::InvalidKey => errno!(ENOENT),
        MapError::EntryExists => errno!(EEXIST),
        MapError::NoMemory => errno!(ENOMEM),
        MapError::SizeLimit => errno!(E2BIG),
        MapError::MapTypeNotSupported | MapError::NotSupported => errno!(ENOSYS),
        MapError::InvalidVmo | MapError::Internal => errno!(EIO),
    }
}

#[derive(Debug, Default)]
struct BpfMapState {
    memory_object: Option<Arc<MemoryObject>>,
    is_frozen: bool,
}

/// A BPF map and Starnix-specific metadata.
#[derive(Debug)]
pub struct BpfMap {
    id: BpfMapId,
    map: PinnedMap,

    /// The internal state of the map object.
    state: OrderedMutex<BpfMapState, EbpfMapStateLevel>,

    /// The security state associated with this bpf Map.
    pub security_state: security::BpfMapState,

    /// Reference to the `Kernel`. Used to unregister `self` on drop.
    kernel: Weak<Kernel>,
}

impl Deref for BpfMap {
    type Target = PinnedMap;
    fn deref(&self) -> &PinnedMap {
        &self.map
    }
}

impl BpfMap {
    pub fn new<L>(
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        schema: MapSchema,
        name: &str,
        security_state: security::BpfMapState,
    ) -> Result<BpfMapHandle, Errno>
    where
        L: LockBefore<EbpfStateLock>,
    {
        let map = Map::new(schema, name).map_err(map_error_to_errno)?;
        let map = BpfMapHandle::new(
            Self {
                id: new_map_id(),
                map,
                state: Default::default(),
                security_state,
                kernel: Arc::downgrade(current_task.kernel()),
            }
            .into(),
        );
        current_task.kernel().ebpf_state.register_map(locked, &map);
        Ok(map)
    }

    pub fn id(&self) -> BpfMapId {
        self.id
    }

    pub(crate) fn frozen<'a, L>(
        &'a self,
        locked: &'a mut Locked<L>,
    ) -> (impl Deref<Target = bool> + 'a, &'a mut Locked<EbpfMapStateLevel>)
    where
        L: LockBefore<EbpfMapStateLevel>,
    {
        let (guard, locked) = self.state.lock_and(locked);
        (MutexGuard::map(guard, |s| &mut s.is_frozen), locked)
    }

    pub(crate) fn freeze<L>(&self, locked: &mut Locked<L>) -> Result<(), Errno>
    where
        L: LockBefore<EbpfMapStateLevel>,
    {
        let mut state = self.state.lock(locked);
        if state.is_frozen {
            return Ok(());
        }
        if let Some(memory) = state.memory_object.take() {
            // The memory has been computed, check whether it is still in use.
            if let Err(memory) = Arc::try_unwrap(memory) {
                // There is other user of the memory. freeze must fail.
                state.memory_object = Some(memory);
                return error!(EBUSY);
            }
        }
        state.is_frozen = true;
        return Ok(());
    }

    pub(crate) fn get_inner(&self) -> PinnedMap {
        self.map.clone()
    }

    pub(crate) fn get_memory<L, F>(
        &self,
        locked: &mut Locked<L>,
        factory: F,
    ) -> Result<Arc<MemoryObject>, Errno>
    where
        L: LockBefore<EbpfMapStateLevel>,
        F: FnOnce() -> Result<Arc<MemoryObject>, Errno>,
    {
        let mut state = self.state.lock(locked);
        if state.is_frozen {
            return error!(EPERM);
        }
        if let Some(memory) = state.memory_object.as_ref() {
            return Ok(memory.clone());
        }
        let memory = factory()?;
        state.memory_object = Some(memory.clone());
        Ok(memory)
    }
}

impl Releasable for BpfMap {
    type Context<'a> = CurrentTaskAndLocked<'a>;

    fn release<'a>(self, (locked, _current_task): CurrentTaskAndLocked<'a>) {
        if let Some(kernel) = self.kernel.upgrade() {
            kernel.ebpf_state.unregister_map(locked, self.id);
        }
    }
}

pub enum BpfMapReleaserAction {}
impl ReleaserAction<BpfMap> for BpfMapReleaserAction {
    fn release(map: ReleaseGuard<BpfMap>) {
        register_delayed_release(map);
    }
}
pub type BpfMapReleaser = ObjectReleaser<BpfMap, BpfMapReleaserAction>;
pub type BpfMapHandle = Arc<BpfMapReleaser>;
pub type WeakBpfMapHandle = Weak<BpfMapReleaser>;
