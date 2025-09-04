// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use fuchsia_inspect_contrib::nodes::BoundedListNode;
pub use std::collections::HashMap;
use std::collections::HashSet;
pub use std::ffi::{CStr, CString};
use std::sync::{LazyLock, Mutex};
use zx::AsHandleRef;
use {fuchsia_inspect as inspect, fuchsia_trace as ftrace, zx};

static CSTR_POOL: LazyLock<Mutex<HashMap<String, &'static CStr>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// Lazily creates &'static CStr values.
//
// Each reference is backed by a CString value that is leaked (and can thus never be deallocated)
// to achieve static lifetime. Each value is indexed by its corresponding `str` value, so a given
// value will only be created once.
//
// This function panics if `s` contains null bytes.
fn lazy_static_cstr(s: &str) -> &'static CStr {
    let mut pool = CSTR_POOL.lock().unwrap();

    // If the string is already in our pool, return the existing CStr.
    if let Some(existing_cstr) = pool.get(s) {
        return existing_cstr;
    }

    // Create the CString and leak it in a box to give it static lifetime.
    let c_string = CString::new(s).expect("Input string cannot contain null bytes");
    let static_cstr: &'static CString = Box::leak(Box::new(c_string));

    pool.insert(s.to_owned(), static_cstr);

    static_cstr
}

/// Map of u32 state IDs to state names.
pub type DiscreteStates = LazyLock<HashMap<u32, &'static CStr>>;

/// Macro to create a DiscreteStates object.
///
/// Example:
///
/// static FAN_SPEEDS: DiscreteStates = discrete_states!(
///   0 => c"OFF",
///   1 => c"LOW",
///   2 => c"HIGH"
/// );
#[macro_export]
macro_rules! discrete_states {
    (
        $($key:expr => $value:expr),*
    ) => {
        $crate::DiscreteStates::new(|| {
            let mut map = $crate::HashMap::new();
            $(
                map.insert($key, $value);
            )*
            map
        })
    };
}

/// Metadata for a discrete state.
///
/// Data is static, with C-formatted strings, for compatibility with tracing.
#[derive(Clone, Debug)]
pub struct DiscreteStateMetadata {
    /// Name of the state.
    /// - Inspect: This name will be used for this state's Inspect node, recorded in
    ///   a node named "power_observability_state_recorders" within the component inspector's root.
    /// - Trace: State transitions will be recorded on a *global* track with this name. If names
    ///   collide, events for the colliding recorders will be placed on the same track.
    pub name: &'static CStr,

    /// Mapping of u32 state IDs to state names.
    pub states: &'static DiscreteStates,

    /// Category for trace events associated with this state.
    pub trace_category: &'static CStr,
}

static ROOT_NODE_NAME: &str = "power_observability_state_recorders";
static ROOT_NODE: LazyLock<inspect::Node> =
    LazyLock::new(|| inspect::component::inspector().root().create_child(ROOT_NODE_NAME));

// Record this process's PID for use in trace track names.
static PID: LazyLock<u64> = LazyLock::new(|| {
    let process = fuchsia_runtime::process_self();
    process.get_koid().expect("failed to get koid").raw_koid()
});

#[derive(thiserror::Error, Debug)]
pub enum StateRecorderError {
    #[error("Name \"{0}\" is not a valid `str`")]
    InvalidName(String),
    #[error("The name \"{0}\" is already in use")]
    DuplicateName(String),
}

static NAMES_IN_USE: LazyLock<Mutex<HashSet<&'static CStr>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

// For convenience, each state name is stored as both a CStr (for tracing) and a str (for Inspect).
struct StateName {
    cstr_name: &'static CStr,
    str_name: &'static str,
}

/// Records state changes to Inspect and trace.
pub struct StateRecorder {
    metadata: DiscreteStateMetadata,
    state_names: HashMap<u32, StateName>,
    transition_history: BoundedListNode,
    _root_node: inspect::Node,
    trace_id: ftrace::Id,
    trace_track_name: &'static CStr,
    trace_state_event: Option<ftrace::AsyncScope>,
}

impl StateRecorder {
    /// Creates a new StateRecorder.
    ///
    /// Errors:
    ///   - StateRecorderError::InvalidName: Either `metadata.name` or a state name contain
    ///     invalid UTF-8 and thus cannot be converted to a str.
    ///   - StateRecorderError::DuplicateName: `metadata.name` is already in use by a StateRecorder
    ///     in this process, returns
    pub fn new(
        metadata: DiscreteStateMetadata,
        initial_state: u32,
        capacity: usize,
    ) -> Result<Self, StateRecorderError> {
        Self::new_with_parent_node(metadata, initial_state, capacity, &ROOT_NODE)
    }

    fn new_with_parent_node(
        metadata: DiscreteStateMetadata,
        initial_state: u32,
        capacity: usize,
        parent: &inspect::Node,
    ) -> Result<Self, StateRecorderError> {
        // Ensure that metadata.name is a valid str.
        match metadata.name.to_str() {
            Ok(_) => {}
            Err(_) => {
                return Err(StateRecorderError::InvalidName(format!("{:?}", metadata.name)));
            }
        };

        match NAMES_IN_USE.lock() {
            Ok(mut names_in_use) => {
                if names_in_use.contains(metadata.name) {
                    return Err(StateRecorderError::DuplicateName(format!("{:?}", metadata.name)));
                }
                names_in_use.insert(metadata.name);
            }
            Err(e) => {
                log::error!(
                    "NAMES_IN_USE is poisoned (error: {:?}); duplicated names will not be detected",
                    e
                );
            }
        }

        // Build up the map of enums to state names, returning an error if any name is not a valid
        // str.
        let mut state_names = HashMap::new();
        for (state_enum, cstr_name) in metadata.states.iter() {
            let state_name = StateName {
                cstr_name,
                str_name: match cstr_name.to_str() {
                    Ok(s) => s,
                    Err(_) => {
                        return Err(StateRecorderError::InvalidName(format!("{:?}", cstr_name)));
                    }
                },
            };
            state_names.insert(*state_enum, state_name);
        }

        let node = parent.create_child(metadata.name.to_string_lossy());

        node.record_child("metadata", |metadata_node| {
            metadata_node.record_string("name", metadata.name.to_string_lossy());
            metadata_node.record_child("states", |states_node| {
                for (state_enum, state_name) in state_names.iter() {
                    states_node.record_uint(state_name.str_name, (*state_enum).into());
                }
            });
        });

        let transition_history =
            BoundedListNode::new(node.create_child("transition_history"), capacity);

        let trace_id = ftrace::Id::random();
        let track_name = lazy_static_cstr(&format!("{} {}", metadata.name.to_string_lossy(), *PID));

        let mut this = Self {
            metadata,
            state_names,
            transition_history,
            _root_node: node,
            trace_id,
            trace_track_name: track_name,
            trace_state_event: None,
        };
        this.record_transition(initial_state);
        Ok(this)
    }

    fn state_name(&self, state_enum: u32) -> &StateName {
        static UNKNOWN_NAME: StateName =
            StateName { cstr_name: c"<Unknown>", str_name: "<Unknown>" };
        self.state_names.get(&state_enum).unwrap_or(&UNKNOWN_NAME)
    }

    pub fn record_transition(&mut self, state_enum: u32) {
        // Clear the trace state event to end the current slice, if one exists.
        self.trace_state_event = None;

        let StateName { cstr_name, str_name } = *self.state_name(state_enum);

        // The async instant must be emitted before the async event begins to name the track
        // according to self.metadata.name.
        ftrace::async_instant!(self.trace_id, self.metadata.trace_category, self.trace_track_name);
        self.trace_state_event =
            ftrace::async_enter!(self.trace_id, self.metadata.trace_category, cstr_name);

        let timestamp = zx::BootInstant::get().into_nanos();
        self.transition_history.add_entry(|node| {
            node.record_int("@time", timestamp);
            node.record_string("value", str_name);
        });
    }
}

impl Drop for StateRecorder {
    fn drop(&mut self) {
        match NAMES_IN_USE.lock() {
            Ok(mut names_in_use) => {
                names_in_use.remove(self.metadata.name);
            }
            Err(e) => {
                log::error!(
                    "NAMES_IN_USE is poisoned (error: {:?}); name {:?} cannot be removed",
                    e,
                    self.metadata.name
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyIntProperty, assert_data_tree};
    use fuchsia_inspect::Inspector;

    static OFF_ON: DiscreteStates = discrete_states!(
        0 => c"OFF",
        1 => c"ON"
    );

    #[fuchsia::test]
    async fn test_off_on() {
        let inspector = Inspector::default();
        let root_node = inspector.root().create_child(ROOT_NODE_NAME);

        let metadata = DiscreteStateMetadata {
            name: c"my_switch",
            states: &OFF_ON,
            trace_category: c"power_test",
        };

        let mut recorder =
            StateRecorder::new_with_parent_node(metadata, 0, 10, &root_node).unwrap();

        recorder.record_transition(1);
        recorder.record_transition(0);
        recorder.record_transition(1);
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
        let inspector = Inspector::default();
        let root_node = inspector.root().create_child(ROOT_NODE_NAME);

        let metadata_0 = DiscreteStateMetadata {
            name: c"switch_0",
            states: &OFF_ON,
            trace_category: c"power_test",
        };

        static DISABLED_ENABLED: DiscreteStates = discrete_states!(
            0 => c"DISABLED",
            1 => c"ENABLED"
        );

        let metadata_1 = DiscreteStateMetadata {
            name: c"switch_1",
            states: &DISABLED_ENABLED,
            trace_category: c"power_test",
        };

        // switch_0 initializes to 0 and transitions to 1, while switch_1 initializes to 1 and
        // transitions to 0, to produce data that is distinct.
        let mut recorder_0 =
            StateRecorder::new_with_parent_node(metadata_0, 0, 10, &root_node).unwrap();
        let mut recorder_1 =
            StateRecorder::new_with_parent_node(metadata_1, 1, 10, &root_node).unwrap();
        recorder_0.record_transition(1);
        recorder_1.record_transition(0);

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
        let inspector = Inspector::default();
        let root_node = inspector.root().create_child(ROOT_NODE_NAME);

        static FAN_SPEEDS: DiscreteStates = discrete_states!(
            0 => c"OFF",
            1 => c"LOW",
            2 => c"HIGH"
        );

        let metadata = DiscreteStateMetadata {
            name: c"the_best_fan",
            states: &FAN_SPEEDS,
            trace_category: c"power_test",
        };

        let mut recorder =
            StateRecorder::new_with_parent_node(metadata, 0, 10, &root_node).unwrap();

        recorder.record_transition(1);
        recorder.record_transition(2);
        recorder.record_transition(0);
        recorder.record_transition(2);
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
        let root_node = inspector.root().create_child(ROOT_NODE_NAME);

        let metadata = DiscreteStateMetadata {
            name: c"my_switch",
            states: &OFF_ON,
            trace_category: c"power_test",
        };

        let recorder =
            StateRecorder::new_with_parent_node(metadata.clone(), 0, 10, &root_node).unwrap();

        // While `recorder` is still in scope, its name cannot be reused.
        assert!(StateRecorder::new_with_parent_node(metadata.clone(), 0, 10, &root_node).is_err());

        // After `recorder` is dropped, its name can be used again.
        drop(recorder);
        assert!(StateRecorder::new_with_parent_node(metadata.clone(), 0, 10, &root_node).is_ok());
    }
}
