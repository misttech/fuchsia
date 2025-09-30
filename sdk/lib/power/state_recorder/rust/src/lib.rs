// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_sync::Mutex;
use std::cmp::Eq;
pub use std::collections::HashMap;
pub use std::ffi::{CStr, CString};
use std::fmt::Display;
use std::hash::Hash;
use std::sync::{Arc, LazyLock};
use strum::IntoEnumIterator;
use zx::AsHandleRef;
use {fuchsia_inspect as inspect, fuchsia_trace as ftrace, zx};

static CSTR_POOL: LazyLock<Mutex<HashMap<String, &'static CStr>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Lazily creates &'static CStr values.
///
/// Each reference is backed by a CString value that is leaked (and can thus never be deallocated)
/// to achieve static lifetime. Each value is indexed by its corresponding `str` value, so a given
/// value will only be created once.
///
/// This function is public for the convenience of clients using Rust versions that predate the
/// introduction of C-string literals in the 2021 edition.
///
/// Errors:
///  - StateRecorderError::IncompatibleString: The provided string could not be converted to a
///    CString.
pub fn lazy_static_cstr(s: &str) -> Result<&'static CStr, StateRecorderError> {
    let mut pool = CSTR_POOL.lock();

    // If the string is already in our pool, return the existing CStr.
    if let Some(existing_cstr) = pool.get(s) {
        return Ok(existing_cstr);
    }

    // Create the CString and leak it in a box to give it static lifetime.
    let c_string =
        CString::new(s).map_err(|_| StateRecorderError::IncompatibleString(s.to_owned()))?;
    let static_cstr: &'static CString = Box::leak(Box::new(c_string));

    pool.insert(s.to_owned(), static_cstr);

    Ok(static_cstr)
}

static ROOT_NODE_NAME: &str = "power_observability_state_recorders";

// StateRecorderManager for use with the singleton inspector.
static SINGLETON_MANAGER: LazyLock<Arc<Mutex<StateRecorderManager>>> =
    LazyLock::new(|| StateRecorderManager::new(inspect::component::inspector()));

// Record this process's PID for use in trace track names.
static PID: LazyLock<u64> = LazyLock::new(|| {
    let process = fuchsia_runtime::process_self();
    process.get_koid().expect("failed to get koid").raw_koid()
});

#[derive(thiserror::Error, Debug)]
pub enum StateRecorderError {
    #[error("The name \"{0}\" is already in use")]
    DuplicateName(String),
    #[error("String \"{0}\" cannot be converted to a CString")]
    IncompatibleString(String),
}

/// Manages the parent node shared by StateRecorder instances, providing protection against name
/// collisions.
pub struct StateRecorderManager {
    pub node: inspect::Node,
    // Represents a set, but implemented using a Vec due to expected small number of elements.
    names_in_use: Vec<String>,
}

impl StateRecorderManager {
    pub fn new(inspector: &inspect::Inspector) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            node: inspector.root().create_child(ROOT_NODE_NAME),
            names_in_use: Vec::new(),
        }))
    }

    fn register_name(&mut self, name: &str) -> Result<(), StateRecorderError> {
        if self.names_in_use.iter().any(|s| s == name) {
            return Err(StateRecorderError::DuplicateName(name.to_owned()));
        }
        self.names_in_use.push(name.to_owned());
        Ok(())
    }

    fn unregister_name(&mut self, name: &str) {
        match self.names_in_use.iter().position(|s| s == name) {
            Some(index) => {
                self.names_in_use.remove(index);
            }
            None => {
                log::error!("unregister_name called with nonexistent name \"{}\"", name);
            }
        }
    }
}

/// Supertrait that combines traits an enum type must satisfy to be compatible with StateRecorder.
pub trait RecordableEnum: Copy + Display + Eq + Hash + IntoEnumIterator + Into<u64> {}
impl<T: Copy + Display + Eq + Hash + IntoEnumIterator + Into<u64>> RecordableEnum for T {}

// To simplify lookups, StateRecorder stores each state name as both CStr (for tracing) and
// String (for Inspect).
struct StateName {
    trace_name: &'static CStr,
    // This is wrapped in an Arc so that StateRecorder can clone a reference to it that is separated
    // from a borrow of `self`.
    //
    // The alternative -- while preserving `Send` for StateRecorder -- would be to wrap
    // StateRecorder::trace_state_event and StateRecorder::transition_history in Mutexes.
    inspect_name: Arc<String>,
}

/// Records state changes to Inspect and trace.
pub struct StateRecorder<T: RecordableEnum> {
    manager: Arc<Mutex<StateRecorderManager>>,
    name: String,
    trace_category: &'static CStr,
    state_names: HashMap<T, StateName>,
    transition_history: BoundedListNode,
    _root_node: inspect::Node,
    trace_id: ftrace::Id,
    trace_track_name: &'static CStr,
    trace_state_event: Option<ftrace::AsyncScope>,
}

impl<T: RecordableEnum> std::fmt::Debug for StateRecorder<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateRecorder")
            .field("metadata", &self.name)
            .field("trace_category", &self.trace_category)
            .field("transition_history", &self.transition_history)
            .finish()
    }
}

impl<T: RecordableEnum> StateRecorder<T> {
    /// Creates a new StateRecorder that records up to`capacity` transitions on a rolling basis. A
    /// record is emitted for `initial_state` upon creation.
    ///
    /// A StateRecorder created by this function is linked to this module's singleton
    /// StateRecorderManager, which in turn corresponds to the singleton Inspector. Any client not
    /// using the singleton Inspector should call `new_with_manager` instead.
    ///
    /// Errors:
    ///   - StateRecorderError::DuplicateName: `metadata.name` is already in use by a StateRecorder
    ///     associated with `manager`.
    ///   - StateRecorderError::IncompatibleString: Either `name` or the display name of a state
    ///     cannot be converted to a CString.

    /// Version of `new` that uses a singleton StateRecorderManager, which is associated with the
    /// singleton inspector.
    pub fn new(
        name: String,
        trace_category: &'static CStr,
        initial_state: T,
        capacity: usize,
    ) -> Result<Self, StateRecorderError> {
        Self::new_with_manager(
            SINGLETON_MANAGER.clone(),
            name,
            trace_category,
            initial_state,
            capacity,
        )
    }

    /// Like `new`, but with a StateRecorderManager provided by the caller.
    pub fn new_with_manager(
        manager: Arc<Mutex<StateRecorderManager>>,
        name: String,
        trace_category: &'static CStr,
        initial_state: T,
        capacity: usize,
    ) -> Result<Self, StateRecorderError> {
        let node = {
            let mut manager = manager.lock();
            if let Err(e) = manager.register_name(&name) {
                return Err(e);
            }
            manager.node.create_child(&name)
        };

        // Build up the map of enums to state names, returning an error if any name is not a valid
        // str.
        let mut state_names = HashMap::new();
        for variant in T::iter() {
            let inspect_name = Arc::new(variant.to_string());
            let trace_name = lazy_static_cstr(&inspect_name)?;
            state_names.insert(variant, StateName { inspect_name, trace_name });
        }

        node.record_child("metadata", |metadata_node| {
            metadata_node.record_string("name", &name);
            metadata_node.record_child("states", |states_node| {
                for (state_enum, state_name) in state_names.iter() {
                    states_node.record_uint(state_name.inspect_name.as_ref(), (*state_enum).into());
                }
            });
        });

        let transition_history =
            BoundedListNode::new(node.create_child("transition_history"), capacity);

        let trace_id = ftrace::Id::random();
        let trace_id_u64: u64 = trace_id.into();
        let trace_track_name = lazy_static_cstr(&format!("{} {} {}", name, *PID, trace_id_u64))?;

        let mut this = Self {
            manager,
            name,
            trace_category,
            state_names,
            transition_history,
            _root_node: node,
            trace_id,
            trace_track_name,
            trace_state_event: None,
        };
        this.record_transition(initial_state);
        Ok(this)
    }

    fn state_name(&self, state_enum: T) -> &StateName {
        static UNKNOWN_NAME: LazyLock<StateName> = LazyLock::new(|| StateName {
            trace_name: c"<Unknown>",
            inspect_name: Arc::new("<Unknown>".to_string()),
        });
        self.state_names.get(&state_enum).unwrap_or(&UNKNOWN_NAME)
    }

    pub fn record_transition(&mut self, state_enum: T) {
        // Clear the trace state event to end the current slice, if one exists.
        self.trace_state_event.take();

        // Clone `inspect_name` so this borrow of `self` can end before the mutable borrows used
        // to modify self.trace_state_event and self.transition_history below.
        let state_name = self.state_name(state_enum);
        let inspect_name = state_name.inspect_name.clone();

        // The async instant must be emitted before the async event begins to name the track
        // according to self.name.
        ftrace::async_instant!(self.trace_id, self.trace_category, self.trace_track_name);

        self.trace_state_event =
            ftrace::async_enter!(self.trace_id, self.trace_category, state_name.trace_name);

        let timestamp = zx::BootInstant::get().into_nanos();

        self.transition_history.add_entry(|node| {
            node.record_int("@time", timestamp);
            node.record_string("value", inspect_name.as_ref());
        });
    }
}

impl<T: RecordableEnum> Drop for StateRecorder<T> {
    fn drop(&mut self) {
        self.manager.lock().unregister_name(&self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyIntProperty, assert_data_tree};
    use fuchsia_inspect::Inspector;
    use strum_macros::{Display, EnumIter};

    #[derive(Copy, Clone, Display, EnumIter, Eq, PartialEq, Hash)]
    #[repr(u8)]
    enum SwitchState {
        OFF = 0,
        ON = 1,
    }

    impl From<SwitchState> for u64 {
        fn from(value: SwitchState) -> Self {
            value as Self
        }
    }

    #[fuchsia::test]
    async fn test_off_on() {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let mut recorder = StateRecorder::new_with_manager(
            manager,
            "my_switch".into(),
            c"power_test",
            SwitchState::OFF,
            10,
        )
        .unwrap();

        recorder.record_transition(SwitchState::ON);
        recorder.record_transition(SwitchState::OFF);
        recorder.record_transition(SwitchState::ON);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                my_switch: {
                    metadata: {
                        name: "my_switch",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    transition_history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                        "2": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "3": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    async fn test_multiple_recorders() {
        #[derive(Copy, Clone, Display, EnumIter, Eq, PartialEq, Hash)]
        #[repr(u8)]
        enum EnablementState {
            DISABLED = 0,
            ENABLED = 1,
        }
        impl From<EnablementState> for u64 {
            fn from(value: EnablementState) -> Self {
                value as Self
            }
        }

        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let mut recorder_0 = StateRecorder::new_with_manager(
            manager.clone(),
            "switch_0".into(),
            c"power_test",
            SwitchState::OFF,
            10,
        )
        .unwrap();
        let mut recorder_1 = StateRecorder::new_with_manager(
            manager,
            "switch_1".into(),
            c"power_test",
            EnablementState::ENABLED,
            10,
        )
        .unwrap();
        recorder_0.record_transition(SwitchState::ON);
        recorder_1.record_transition(EnablementState::DISABLED);

        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                switch_0: {
                    metadata: {
                        name: "switch_0",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    transition_history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    }
                },
               switch_1: {
                    metadata: {
                        name: "switch_1",
                        states: {
                            "DISABLED": 0u64,
                            "ENABLED": 1u64,
                        }
                    },
                    transition_history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "ENABLED",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "DISABLED",
                        },
                    }
                }
            }
        })
    }

    #[fuchsia::test]
    async fn test_three_states() {
        #[derive(Copy, Clone, Display, EnumIter, Eq, PartialEq, Hash)]
        #[repr(u8)]
        enum FanSpeed {
            OFF = 0,
            LOW = 1,
            HIGH = 2,
        }

        impl From<FanSpeed> for u64 {
            fn from(value: FanSpeed) -> Self {
                value as Self
            }
        }

        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let mut recorder = StateRecorder::new_with_manager(
            manager,
            "the_best_fan".into(),
            c"power_test",
            FanSpeed::OFF,
            10,
        )
        .unwrap();

        recorder.record_transition(FanSpeed::LOW);
        recorder.record_transition(FanSpeed::HIGH);
        recorder.record_transition(FanSpeed::OFF);
        recorder.record_transition(FanSpeed::HIGH);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                the_best_fan: {
                    metadata: {
                        name: "the_best_fan",
                        states: {
                            "OFF": 0u64,
                            "LOW": 1u64,
                            "HIGH": 2u64,
                        }
                    },
                    transition_history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "LOW",
                        },
                        "2": {
                            "@time": AnyIntProperty,
                            "value": "HIGH",
                        },
                        "3": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "4": {
                            "@time": AnyIntProperty,
                            "value": "HIGH",
                        },
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    async fn test_name_reuse_not_allowed() {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let recorder = StateRecorder::new_with_manager(
            manager.clone(),
            "my_switch".into(),
            c"power_test",
            SwitchState::OFF,
            10,
        )
        .unwrap();

        // While `recorder` is still in scope, its name cannot be reused.
        let result = StateRecorder::new_with_manager(
            manager.clone(),
            "my_switch".into(),
            c"power_test",
            SwitchState::OFF,
            10,
        );
        assert!(result.is_err());

        // After `recorder` is dropped, its name can be used again.
        drop(recorder);
        let result = StateRecorder::new_with_manager(
            manager.clone(),
            "my_switch".into(),
            c"power_test",
            SwitchState::OFF,
            10,
        );
        assert!(result.is_ok());
    }

    #[fuchsia::test]
    async fn test_singleton_manager() {
        let mut recorder =
            StateRecorder::new("my_switch".into(), c"power_test", SwitchState::OFF, 10).unwrap();

        recorder.record_transition(SwitchState::ON);
        assert_data_tree!(inspect::component::inspector(), root: {
            power_observability_state_recorders: {
                my_switch: {
                    metadata: {
                        name: "my_switch",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    transition_history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    async fn test_recorder_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<StateRecorder<SwitchState>>();
    }
}
