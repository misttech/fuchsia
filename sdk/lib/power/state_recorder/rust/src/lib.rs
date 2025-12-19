// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Standardized reporting of time series data via Inspect and trace. It supports recording of
//! **enum states** and **numeric states**.
//!
//! For example use, see the [example code][strc].
//!
//! For the intro to the library, see the [README.md][rdme].
//!
//! [rdme]: https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/lib/power/state_recorder/README.md
//! [strc]: https://cs.opensource.google/fuchsia/fuchsia/+/main:examples/power/state_recorder
//!

use fuchsia_inspect::Inspector;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_sync::Mutex;
use futures_util::FutureExt;
use std::cmp::Eq;
pub use std::collections::HashMap;
pub use std::ffi::{CStr, CString};
use std::fmt::{Debug, Display};
use std::fs::{self as fs, OpenOptions};
use std::hash::Hash;
use std::io::Write as OtherWrite;
use std::marker::PhantomData;
use std::path::Path;
use std::str::FromStr;
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

pub fn manager() -> Arc<Mutex<StateRecorderManager>> {
    SINGLETON_MANAGER.clone()
}

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
    #[error("Invalid options: {0}")]
    InvalidOptions(String),
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
pub trait RecordableEnum:
    Copy + Debug + Display + FromStr + Eq + Hash + IntoEnumIterator + Into<u64> + Send + Sync
{
}
impl<T: Copy + Debug + Display + FromStr + Eq + Hash + IntoEnumIterator + Into<u64> + Send + Sync>
    RecordableEnum for T
{
}

// To simplify lookups, StateRecorder stores each state name as both CStr (for tracing) and
// String (for Inspect).
struct StateName {
    trace_name: &'static CStr,
    // This is wrapped in an Arc so that StateRecorder can clone a reference to it that is separated
    // from a borrow of `self`.
    //
    // The alternative -- while preserving `Send` for StateRecorder -- would be to wrap
    // StateRecorder::trace_state_event and StateRecorder::history in Mutexes.
    inspect_name: Arc<String>,
}

/// Records time series data for an enum-valued state. This is best-suited for categorical
/// observations, where the name of the state and not a numeric value will be most relevant for
/// diagnostic and forensic purposes.
pub struct EnumStateRecorder<T: RecordableEnum> {
    manager: Arc<Mutex<StateRecorderManager>>,
    name: String,
    trace_category: &'static CStr,
    state_names: HashMap<T, StateName>,
    history: RecorderHistory<T>,
    persistence: Option<PersistenceHandler<T>>,
    _root_node: inspect::Node,
    trace_id: ftrace::Id,
    trace_track_name: &'static CStr,
    trace_state_event: Option<ftrace::AsyncScope<&'static CStr, &'static CStr>>,
}

impl<T: RecordableEnum> std::fmt::Debug for EnumStateRecorder<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateRecorder")
            .field("metadata", &self.name)
            .field("trace_category", &self.trace_category)
            .field("history", &self.history)
            .finish()
    }
}

impl<T: RecordableEnum + 'static> EnumStateRecorder<T> {
    /// Creates a new EnumStateRecorder that records up to `capacity` state values on a rolling
    /// basis.
    ///
    /// An EnumStateRecorder created by this function is linked to this module's singleton
    /// StateRecorderManager, which in turn corresponds to the singleton Inspector. Any client not
    /// using the singleton Inspector should call `new_with_manager` instead.
    ///
    /// Errors:
    ///   - StateRecorderError::DuplicateName: `metadata.name` is already in use by a StateRecorder
    ///     associated with `manager`.
    ///   - StateRecorderError::IncompatibleString: Either `name` or the display name of a state
    ///     cannot be converted to a CString.
    ///   - StateRecorderError::InvalidOptions: `options` is invalid for the given mode.
    pub fn new(
        name: String,
        trace_category: &'static CStr,
        options: RecorderOptions,
    ) -> Result<Self, StateRecorderError> {
        let manager = options.manager.unwrap_or_else(|| SINGLETON_MANAGER.clone());
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
            metadata_node.record_string("type", "enum");
            metadata_node.record_child("states", |states_node| {
                for (state_enum, state_name) in state_names.iter() {
                    states_node.record_uint(state_name.inspect_name.as_ref(), (*state_enum).into());
                }
            });
        });

        let mut persistence_handler = None;

        let history = if options.lazy_record {
            let shared_buffer = if let Some(config) = options.persistence {
                let (handler, buffer) = PersistenceHandler::new(config.clone(), options.capacity);

                // Handle Previous Boot Node (Static read from disk)
                let prev_data = PersistenceHandler::<T>::read_log(&config.previous_path);

                if !prev_data.is_empty() {
                    let data_arc = Arc::new(prev_data);
                    node.record_lazy_child("previous_boot_history", move || {
                        let data = data_arc.clone();
                        async move {
                            let inspector = Inspector::default();
                            let root = inspector.root();
                            for (i, (ts, val)) in data.iter().enumerate() {
                                root.record_child(i.to_string(), |child| {
                                    child.record_int("@time", *ts);
                                    child.record_string("value", val.to_string());
                                });
                            }
                            Ok(inspector)
                        }
                        .boxed()
                    });
                }

                persistence_handler = Some(handler);
                buffer
            } else {
                Arc::new(Mutex::new(TimestampRingBuffer::<T>::with_capacity(options.capacity)))
            };

            // "reset_info"
            let buffer_cloned = shared_buffer.clone();
            node.record_lazy_child("reset_info", move || {
                let history = buffer_cloned.clone();
                async move {
                    let inspector = Inspector::default();
                    let node = inspector.root();
                    let (count, last_reset_ns) = history.lock().get_reset_info();
                    node.record_int("count", count as i64);
                    node.record_int("last_reset_ns", last_reset_ns);
                    Ok(inspector)
                }
                .boxed()
            });
            let buffer_cloned = shared_buffer.clone();
            node.record_lazy_child("history", move || {
                let history = buffer_cloned.clone();
                async move {
                    let inspector = Inspector::default();
                    let node = inspector.root();
                    for (i, (timestamp, state_enum)) in history.lock().iter().enumerate() {
                        node.record_child(format!("{}", i), |node| {
                            node.record_int("@time", timestamp);
                            node.record_string("value", state_enum.to_string());
                        });
                    }
                    Ok(inspector)
                }
                .boxed()
            });

            RecorderHistory::Lazy(shared_buffer)
        } else {
            // --- EAGER MODE (Persistence NOT Supported for now) ---
            if options.persistence.is_some() {
                return Err(StateRecorderError::InvalidOptions(
                    "Persistence not supported in eager mode".to_string(),
                ));
            }

            node.record_child("reset_info", |node| {
                node.record_int("count", 0);
                node.record_int("last_reset_ns", zx::BootInstant::get().into_nanos());
            });

            let history_node = BoundedListNode::new(node.create_child("history"), options.capacity);

            RecorderHistory::Eager(history_node)
        };

        let trace_id = ftrace::Id::random();
        let trace_id_u64: u64 = trace_id.into();
        let trace_track_name = lazy_static_cstr(&format!("{} {} {}", name, *PID, trace_id_u64))?;

        Ok(Self {
            manager,
            name,
            trace_category,
            state_names,
            history,
            persistence: persistence_handler,
            _root_node: node,
            trace_id,
            trace_track_name,
            trace_state_event: None,
        })
    }

    fn state_name(&self, state_enum: T) -> &StateName {
        static UNKNOWN_NAME: LazyLock<StateName> = LazyLock::new(|| StateName {
            trace_name: c"<Unknown>",
            inspect_name: Arc::new("<Unknown>".to_string()),
        });
        self.state_names.get(&state_enum).unwrap_or(&UNKNOWN_NAME)
    }

    pub fn record(&mut self, state_enum: T) {
        // Clear the trace state event to end the current slice, if one exists.
        self.trace_state_event.take();

        // Clone `inspect_name` so this borrow of `self` can end before the mutable borrows used
        // to modify self.trace_state_event and self.history below.
        let state_name = self.state_name(state_enum);
        let inspect_name = state_name.inspect_name.clone();

        // The async instant must be emitted before the async event begins to name the track
        // according to self.name.
        ftrace::async_instant!(self.trace_id, self.trace_category, self.trace_track_name);

        self.trace_state_event =
            ftrace::async_enter!(self.trace_id, self.trace_category, state_name.trace_name);

        let timestamp = zx::BootInstant::get().into_nanos();

        // If Persistence is on (Lazy), the handler OWNS the buffer update.
        if let Some(handler) = &mut self.persistence {
            // Updates the shared buffer inside its lock and handle persistence.
            handler.append(timestamp, state_enum);
        } else {
            // Update manually
            match &mut self.history {
                RecorderHistory::Eager(history) => {
                    history.add_entry(|node| {
                        node.record_int("@time", timestamp);
                        node.record_string("value", inspect_name.as_ref());
                    });
                }
                RecorderHistory::Lazy(history) => {
                    history.lock().insert(timestamp, state_enum);
                }
            }
        }
    }
}

impl<T: RecordableEnum> Drop for EnumStateRecorder<T> {
    fn drop(&mut self) {
        self.manager.lock().unregister_name(&self.name);
    }
}

/// To be recordable, a numeric type must, in essence, be able to widen into a trace-compatible
/// type and an Inspect-compatible type. Users are not expected to implement this trait; this
/// module implements it for common numeric types below.
pub trait RecordableNumericType:
    Copy + Debug + Display + FromStr + Sized + Send + Sync + 'static
{
    type TraceType: ftrace::ArgValue;

    fn trace_value(&self) -> Self::TraceType;
    fn record(&self, node: &inspect::Node, name: &str);
    fn record_range(range: &(Self, Self), node: &inspect::Node);
}

macro_rules! impl_recordable_numeric_type {
    ($numeric_type:ty, $trace_type:ty, u64) => {
        impl RecordableNumericType for $numeric_type {
            type TraceType = $trace_type;

            fn trace_value(&self) -> Self::TraceType {
                *self as Self::TraceType
            }
            fn record(&self, node: &inspect::Node, name: &str) {
                node.record_uint(name, *self as u64);
            }
            fn record_range(range: &(Self, Self), node: &inspect::Node) {
                node.record_uint("min_inc", range.0 as u64);
                node.record_uint("max_inc", range.1 as u64);
            }
        }
    };
    ($numeric_type:ty, $trace_type:ty, i64) => {
        impl RecordableNumericType for $numeric_type {
            type TraceType = $trace_type;

            fn trace_value(&self) -> Self::TraceType {
                *self as Self::TraceType
            }
            fn record(&self, node: &inspect::Node, name: &str) {
                node.record_int(name, *self as i64);
            }
            fn record_range(range: &(Self, Self), node: &inspect::Node) {
                node.record_int("min_inc", range.0 as i64);
                node.record_int("max_inc", range.1 as i64);
            }
        }
    };
    ($numeric_type:ty, $trace_type:ty, f64) => {
        impl RecordableNumericType for $numeric_type {
            type TraceType = $trace_type;

            fn trace_value(&self) -> Self::TraceType {
                *self as Self::TraceType
            }
            fn record(&self, node: &inspect::Node, name: &str) {
                node.record_double(name, *self as f64);
            }
            fn record_range(range: &(Self, Self), node: &inspect::Node) {
                node.record_double("min_inc", range.0 as f64);
                node.record_double("max_inc", range.1 as f64);
            }
        }
    };
}

impl_recordable_numeric_type!(u8, u32, u64);
impl_recordable_numeric_type!(u16, u32, u64);
impl_recordable_numeric_type!(u32, u32, u64);
impl_recordable_numeric_type!(u64, u64, u64);
impl_recordable_numeric_type!(i8, i32, i64);
impl_recordable_numeric_type!(i16, i32, i64);
impl_recordable_numeric_type!(i32, i32, i64);
impl_recordable_numeric_type!(i64, i64, i64);
impl_recordable_numeric_type!(f32, f64, f64);
impl_recordable_numeric_type!(f64, f64, f64);

/// Units supported by NumericStateRecorder. The `units!` macro is recommended for construction.
///
/// Bytes and bit-rates are specifically not included yet because they invite the question of
/// whether they should be restricted to binary prefixes. We'll address that once we instrument a
/// specific use case.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Units {
    Amps(Option<DecimalPrefix>),
    AmpHours(Option<DecimalPrefix>),
    Hertz(Option<DecimalPrefix>),
    Joules(Option<DecimalPrefix>),
    Watts(Option<DecimalPrefix>),
    Volts(Option<DecimalPrefix>),
    Celsius(Option<DecimalPrefix>),
    Number(Option<DecimalPrefix>),
    Percent,
}

impl Display for Units {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn write_helper(
            f: &mut std::fmt::Formatter<'_>,
            prefix: &Option<DecimalPrefix>,
            unit_str: &str,
        ) -> std::fmt::Result {
            match prefix {
                Some(p) => write!(f, "{}{}", p, unit_str),
                None => write!(f, "{}", unit_str),
            }
        }

        match self {
            Units::Amps(prefix) => write_helper(f, prefix, "A"),
            Units::AmpHours(prefix) => write_helper(f, prefix, "AH"),
            Units::Hertz(prefix) => write_helper(f, prefix, "Hz"),
            Units::Joules(prefix) => write_helper(f, prefix, "J"),
            Units::Watts(prefix) => write_helper(f, prefix, "W"),
            Units::Volts(prefix) => write_helper(f, prefix, "V"),
            Units::Celsius(prefix) => write_helper(f, prefix, "C"),
            Units::Number(prefix) => write_helper(f, prefix, "#"),
            Units::Percent => write!(f, "%"),
        }
    }
}

/// Decimal prefixes for use with certain `Units`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecimalPrefix {
    Nano,
    Micro,
    Milli,
    Centi,
    Deci,
    Kilo,
    Mega,
    Giga,
}

impl Display for DecimalPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecimalPrefix::Nano => write!(f, "n"),
            DecimalPrefix::Micro => write!(f, "u"),
            DecimalPrefix::Milli => write!(f, "m"),
            DecimalPrefix::Centi => write!(f, "c"),
            DecimalPrefix::Deci => write!(f, "d"),
            DecimalPrefix::Kilo => write!(f, "k"),
            DecimalPrefix::Mega => write!(f, "M"),
            DecimalPrefix::Giga => write!(f, "G"),
        }
    }
}

/// Assembles fully-specified measurement units for NumericStateRecorder, combining a base unit
/// with an optional prefix.
///
/// Examples:
///     - units!(Volt)
///     - units!(Percent)
///     - units!(Kilo, Hertz)
///     - units!(Milli, Amp)
#[macro_export]
macro_rules! units {
    (Percent) => {
        $crate::Units::Percent
    };
    ($base_unit:ident) => {
        $crate::Units::$base_unit(None)
    };
    ($prefix:ident, $base_unit:ident) => {
        $crate::Units::$base_unit(Some($crate::DecimalPrefix::$prefix))
    };
}

// Holds information for persistence
#[derive(Clone, Debug)]
pub struct PersistenceOptions {
    /// Unique name for this recorder (e.g., "battery_level").
    name: String,
    /// For current history.
    current_path: String,
    /// For previous history.
    previous_path: String,
    // A temporary name for current history to achieve atomic persistence.
    rename_path: String,
}

impl PersistenceOptions {
    // Unique name and path to storage and path to volatile directory.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            current_path: format!("/data/{}.csv", name),
            previous_path: format!("/tmp/{}.csv", name),
            rename_path: format!("/data/{}.tmp", name),
            name,
        }
    }

    pub fn storage_dir(mut self, dir: &str) -> Self {
        self.current_path = format!("{}/{}.csv", dir, self.name);
        self.rename_path = format!("{}/{}.tmp", dir, self.name);
        self
    }

    pub fn volatile_dir(mut self, dir: &str) -> Self {
        self.previous_path = format!("{}/{}.csv", dir, self.name);
        self
    }
    // Helper to generate paths
    fn paths(&self) -> (&str, &str, &str) {
        (&self.current_path, &self.previous_path, &self.rename_path)
    }
}

/// Handles persistence using TimestampRingBuffer as the backing store to save memory.
struct PersistenceHandler<T: Copy> {
    config: PersistenceOptions,
    // We reuse TimestampRingBuffer for memory-optimized storage (i32 offsets)
    buffer: Arc<Mutex<TimestampRingBuffer<T>>>,
}

impl<T: Copy + FromStr + Display> PersistenceHandler<T> {
    fn new(
        config: PersistenceOptions,
        capacity: usize,
    ) -> (Self, Arc<Mutex<TimestampRingBuffer<T>>>) {
        let (curr, prev, _) = config.paths();

        // Perform rotation before loading data
        Self::prepare_files(&curr, &prev);

        // Load any data remaining in 'current' (crash recovery)
        let initial_data = Self::read_log(&curr);

        // 3. Hydrate our internal ring buffer
        let mut buffer = TimestampRingBuffer::with_capacity(capacity);
        for (ts, val) in initial_data {
            buffer.insert(ts, val);
        }

        let shared_buffer = Arc::new(Mutex::new(buffer));

        (Self { config, buffer: shared_buffer.clone() }, shared_buffer)
    }

    /// Handles the rotation logic:
    /// If PREV doesn't exist (reboot), move CURR to PREV.
    /// If PREV exists (crash), leave CURR alone (it contains valuable pre-crash data).
    fn prepare_files(curr_path: &str, prev_path: &str) {
        if Path::new(prev_path).exists() {
            // Previous file exists -> Crash recovery.
            // Do not overwrite it. Do not touch current file.
            log::warn!("Not moving history, {} already exists", prev_path);
            return;
        }

        // Move content by reading then writing it for moves from /data to /tmp.
        let Ok(content) = std::fs::read_to_string(curr_path).map_err(|e| {
            log::info!("Could not read current history, not moving: {}", e);
        }) else {
            return;
        };

        if let Err(e) = std::fs::write(prev_path, &content) {
            log::warn!("Could not write previous boot history: {}", e);
            return;
        }

        if let Err(e) = std::fs::File::create(curr_path) {
            log::warn!("Could not clear current boot history: {}", e);
        }
    }

    fn flush(&self, buffer_guard: &TimestampRingBuffer<T>) {
        let (curr, _, temp) = self.config.paths();
        let try_write = || -> std::io::Result<()> {
            let mut file =
                OpenOptions::new().write(true).create(true).truncate(true).open(&temp)?;

            // Iterate the ring buffer (which converts internal 32-bit offsets back to 64-bit TS)
            for (ts, val) in buffer_guard.iter() {
                writeln!(file, "{},{}", ts, val)?;
            }

            file.sync_data()?;
            fs::rename(&temp, &curr)?;
            Ok(())
        };

        if let Err(e) = try_write() {
            log::error!("StateRecorder: Persist failed for {}: {:?}", self.config.name, e);
        }
    }

    /// Appends data to memory and syncs to disk.
    fn append(&mut self, timestamp: i64, value: T) {
        let mut guard = self.buffer.lock();
        guard.insert(timestamp, value);
        self.flush(&guard);
    }

    /// Static helper to read log from disk into a vector.
    fn read_log(path: &str) -> Vec<(i64, T)> {
        let Ok(content) = fs::read_to_string(path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let mut parts = line.splitn(2, ',');
                let ts = parts.next()?.trim().parse::<i64>().ok()?;
                let val = parts.next()?.trim().parse::<T>().ok()?;
                Some((ts, val))
            })
            .collect()
    }
}

/// Options for NumericStateRecorder and EnumStateRecorder
#[derive(Default)]
pub struct RecorderOptions {
    // If true, recorder will lazily record values to inspect. Otherwise, will record eagerly.
    pub lazy_record: bool,
    /// Maximum number of recorded values to store on a rolling basis.
    pub capacity: usize,
    /// Optional. If not set, the Recorder will be linked to this module's singleton
    /// StateRecorderManager, which in turn corresponds to the singleton Inspector.
    /// If set, the manager supplied here will be used.
    pub manager: Option<Arc<Mutex<StateRecorderManager>>>,
    // Optional persistence config
    pub persistence: Option<PersistenceOptions>,
}

#[derive(Debug)]
enum RecorderHistory<T: Copy + Debug> {
    Eager(BoundedListNode),
    Lazy(Arc<Mutex<TimestampRingBuffer<T>>>),
}

#[derive(Debug)]
/// A fixed-size ring buffer with timestamps for each insertion.
/// All input and output are in nanoseconds, but will be rounded down to
/// the nearest millisecond and stored as milliseconds internally.
/// When the capacity is reached, insertions will wrap around and continue
/// from the beginning of the buffer. There is a maximum delta of ~24.8 days
/// between insertions. If this maximum is exceeded, the buffer will drop
/// all data except for the newest insertion.
struct TimestampRingBuffer<T: Copy> {
    /// Initial timestamp in milliseconds, used as basis for offsets.
    start_timestamp_ms: i64,
    /// Last timestamp inserted, in milliseconds.
    last_timestamp_ms: i64,
    /// Index where the next element should be inserted.
    next_index: usize,
    /// Store timestamps as millisecond offsets from `last_timestamp_ms`.
    offset_ms: Vec<i32>,
    /// Data to be stored in the buffer.
    data: Vec<T>,
    /// Number of times the buffer has been reset (due to max delta exceeded).
    reset_count: u32,
    /// Timestamp of the last buffer reset
    last_reset_ms: i64,
}

const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;

fn ms_to_ns(ms: i64) -> i64 {
    ms * NANOSECONDS_PER_MILLISECOND
}

fn ns_to_ms(ns: i64) -> i64 {
    ns / NANOSECONDS_PER_MILLISECOND
}

impl<T: Copy> TimestampRingBuffer<T> {
    fn with_capacity(capacity: usize) -> Self {
        let now_ms = ns_to_ms(zx::BootInstant::get().into_nanos());
        Self {
            start_timestamp_ms: now_ms,
            last_timestamp_ms: now_ms,
            next_index: 0,
            offset_ms: Vec::with_capacity(capacity),
            data: Vec::with_capacity(capacity),
            reset_count: 0,
            last_reset_ms: now_ms,
        }
    }

    fn insert(&mut self, timestamp_ns: i64, value: T) {
        let timestamp_ms = ns_to_ms(timestamp_ns);
        // Attempt to down-convert the offset from last_timestamp_ms to an i32
        let offset_ms = match i32::try_from(timestamp_ms - self.last_timestamp_ms) {
            Ok(offset_ms) => offset_ms,
            Err(_) => {
                // Offset from last_timestamp_ms exceeds maximum allowable,
                // reset the buffer.
                self.offset_ms.clear();
                self.data.clear();
                self.start_timestamp_ms = timestamp_ms;
                self.next_index = 0;
                self.reset_count += 1;
                self.last_reset_ms = self.start_timestamp_ms;
                0
            }
        };
        if self.offset_ms.len() < self.offset_ms.capacity() {
            // Buffer isn't full yet, just append.
            self.offset_ms.push(offset_ms);
            self.data.push(value);
        } else {
            // Buffer is full, shift `start_timestamp_ms` forward by the oldest
            // offset, then overwrite that entry with the new data.
            self.start_timestamp_ms += self.offset_ms[self.next_index] as i64;
            self.offset_ms[self.next_index] = offset_ms;
            self.data[self.next_index] = value;
        }
        self.last_timestamp_ms = timestamp_ms;
        self.next_index = (self.next_index + 1) % self.offset_ms.capacity();
    }

    /// Returns the reset count, and the timestamp of the last reset in nanoseconds.
    fn get_reset_info(&self) -> (u32, i64) {
        (self.reset_count, ms_to_ns(self.last_reset_ms))
    }

    /// Returns an Iterator of (timestamp in nanoseconds, T), starting
    /// from the oldest entry.
    fn iter(&self) -> TimestampRingBufferIter<'_, T> {
        TimestampRingBufferIter::new(self)
    }
}

struct TimestampRingBufferIter<'a, T: Copy> {
    buffer: &'a TimestampRingBuffer<T>,
    index: usize,
    last_timestamp_ms: i64,
}

impl<'a, T: Copy> TimestampRingBufferIter<'a, T> {
    fn new(buffer: &'a TimestampRingBuffer<T>) -> Self {
        Self { buffer, index: 0, last_timestamp_ms: buffer.start_timestamp_ms }
    }
}

/// Iterate over the wrapped buffer, returning (timestamp in nanoseconds, T),
/// starting from the oldest entry.
impl<T: Copy> Iterator for TimestampRingBufferIter<'_, T> {
    type Item = (i64, T);

    fn next(&mut self) -> Option<(i64, T)> {
        if self.index >= self.buffer.offset_ms.len() {
            return None;
        }
        // Start from the oldest insertion and wrap around.
        let index = (self.index + self.buffer.next_index) % self.buffer.offset_ms.len();
        let timestamp_ms = self.last_timestamp_ms + self.buffer.offset_ms[index] as i64;
        self.index += 1;
        self.last_timestamp_ms = timestamp_ms;
        Some((ms_to_ns(timestamp_ms), self.buffer.data[index]))
    }
}

pub struct NumericStateRecorder<T: RecordableNumericType> {
    manager: Arc<Mutex<StateRecorderManager>>,
    name: String,
    trace_category: &'static CStr,
    trace_name: &'static CStr,
    units: String,
    history: RecorderHistory<T>,
    persistence: Option<PersistenceHandler<T>>,
    _root_node: inspect::Node,
    trace_id: ftrace::Id,
    _phantom: PhantomData<T>,
}

impl<T: RecordableNumericType> NumericStateRecorder<T> {
    /// Creates a new NumericStateRecorder.
    ///
    /// See `RecorderOptions` for more details on options that can be specified.
    ///
    /// Errors:
    ///   - StateRecorderError::DuplicateName: `metadata.name` is already in use by a StateRecorder
    ///     associated with `manager`.
    ///   - StateRecorderError::IncompatibleString: Either `name` or the display name of a state
    ///     cannot be converted to a CString.
    ///   - StateRecorderError::InvalidOptions: `options` is invalid for the given mode.
    pub fn new(
        name: String,
        trace_category: &'static CStr,
        units: Units,
        range: Option<(T, T)>,
        options: RecorderOptions,
    ) -> Result<Self, StateRecorderError> {
        let manager = options.manager.unwrap_or_else(|| SINGLETON_MANAGER.clone());
        let node = {
            let mut manager = manager.lock();
            if let Err(e) = manager.register_name(&name) {
                return Err(e);
            }
            manager.node.create_child(&name)
        };

        let trace_name = lazy_static_cstr(&name)?;
        let units = format!("{}", units);

        node.record_child("metadata", |metadata_node| {
            metadata_node.record_string("name", &name);
            metadata_node.record_string("type", "numeric");
            metadata_node.record_string("units", &units);
            match range {
                Some(r) => metadata_node.record_child("range", |node| T::record_range(&r, node)),
                None => metadata_node.record_string("range", "<Unspecified>"),
            }
        });

        let mut persistence_handler = None;
        let history = if options.lazy_record {
            // --- LAZY MODE (Shared Buffer) ---

            let shared_buffer = if let Some(config) = options.persistence {
                let (handler, buffer) = PersistenceHandler::new(config.clone(), options.capacity);

                // Handle Previous Boot Node (Static read from disk)
                let prev_data = PersistenceHandler::<T>::read_log(&config.previous_path);

                if !prev_data.is_empty() {
                    let data_arc = Arc::new(prev_data);
                    node.record_lazy_child("previous_boot_history", move || {
                        let data = data_arc.clone();
                        async move {
                            let inspector = Inspector::default();
                            let root = inspector.root();
                            for (i, (ts, val)) in data.iter().enumerate() {
                                root.record_child(i.to_string(), |child| {
                                    child.record_int("@time", *ts);
                                    // Use generic numeric record trait method
                                    val.record(child, "value");
                                });
                            }
                            Ok(inspector)
                        }
                        .boxed()
                    });
                }

                persistence_handler = Some(handler);
                buffer
            } else {
                Arc::new(Mutex::new(TimestampRingBuffer::<T>::with_capacity(options.capacity)))
            };

            // "reset_info"
            let buffer_cloned = shared_buffer.clone();
            node.record_lazy_child("reset_info", move || {
                let history = buffer_cloned.clone();
                async move {
                    let inspector = Inspector::default();
                    let node = inspector.root();
                    let (count, last_reset_ns) = history.lock().get_reset_info();
                    node.record_int("count", count as i64);
                    node.record_int("last_reset_ns", last_reset_ns);
                    Ok(inspector)
                }
                .boxed()
            });

            // "history"
            let buffer_cloned = shared_buffer.clone();
            node.record_lazy_child("history", move || {
                let history = buffer_cloned.clone();
                async move {
                    let inspector = Inspector::default();
                    let node = inspector.root();
                    for (i, (timestamp, state_value)) in history.lock().iter().enumerate() {
                        node.record_child(format!("{}", i), |node| {
                            node.record_int("@time", timestamp);
                            // Numeric-specific record
                            state_value.record(&node, "value");
                        });
                    }
                    Ok(inspector)
                }
                .boxed()
            });

            RecorderHistory::Lazy(shared_buffer)
        } else {
            // --- EAGER MODE (Persistence NOT Supported for now) ---
            if options.persistence.is_some() {
                return Err(StateRecorderError::InvalidOptions(
                    "Persistence not supported in eager mode".to_string(),
                ));
            }

            node.record_child("reset_info", |node| {
                node.record_int("count", 0);
                node.record_int("last_reset_ns", zx::BootInstant::get().into_nanos());
            });

            let history_node = BoundedListNode::new(node.create_child("history"), options.capacity);

            RecorderHistory::Eager(history_node)
        };

        Ok(Self {
            manager,
            name,
            trace_category,
            trace_name,
            units,
            history,
            persistence: persistence_handler,
            _root_node: node,
            trace_id: ftrace::Id::random(),
            _phantom: PhantomData,
        })
    }

    pub fn record(&mut self, state_value: T) {
        let timestamp = zx::BootInstant::get().into_nanos();

        ftrace::counter!(
            self.trace_category,
            self.trace_name,
            self.trace_id.into(),
            &self.units.to_string() => state_value.trace_value()
        );

        // If Persistence is on (Lazy), the handler OWNS the shared buffer update.
        if let Some(handler) = &mut self.persistence {
            handler.append(timestamp, state_value);
        } else {
            match &mut self.history {
                RecorderHistory::Eager(history) => {
                    history.add_entry(|node| {
                        node.record_int("@time", timestamp);
                        state_value.record(node, "value");
                    });
                }
                RecorderHistory::Lazy(history) => {
                    history.lock().insert(timestamp, state_value);
                }
            }
        }
    }
}

impl<T: RecordableNumericType> Drop for NumericStateRecorder<T> {
    fn drop(&mut self) {
        self.manager.lock().unregister_name(&self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyIntProperty, assert_data_tree};
    use fuchsia_inspect::Inspector;
    use strum_macros::{Display, EnumIter, EnumString};
    use test_case::test_case;

    #[derive(Copy, Clone, Debug, Display, EnumIter, EnumString, Eq, PartialEq, Hash)]
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
    async fn test_timestamp_ring_buffer() {
        let mut buffer = TimestampRingBuffer::<i32>::with_capacity(3);
        let start_ms = buffer.start_timestamp_ms;

        let t1 = (ms_to_ns(start_ms + 1000), 1);
        // t2's timestamp is before t1, which will result in a negative offset.
        let t2 = (ms_to_ns(start_ms + 900), 2);
        let t3 = (ms_to_ns(start_ms + 3000), 3);

        buffer.insert(t1.0, t1.1);
        buffer.insert(t2.0, t2.1);
        buffer.insert(t3.0, t3.1);

        assert_eq!(vec![t1, t2, t3], buffer.iter().collect::<Vec<_>>());
        assert_eq!((0, ms_to_ns(start_ms)), buffer.get_reset_info());

        // Buffer is already at capacity, so this should overwrite the first element.
        let t4 = (ms_to_ns(start_ms + 4000), 4);
        buffer.insert(t4.0, t4.1);
        assert_eq!(vec![t2, t3, t4], buffer.iter().collect::<Vec<_>>());
        assert_eq!((0, ms_to_ns(start_ms)), buffer.get_reset_info());
    }

    #[fuchsia::test]
    async fn test_timestamp_ring_buffer_resets_on_maximum_offset() {
        let mut buffer = TimestampRingBuffer::<i32>::with_capacity(3);
        let start_ms = buffer.start_timestamp_ms;

        const MAX_OFFSET_MS: i64 = i32::MAX as i64;
        let t1 = (ms_to_ns(start_ms + 1000), 1);
        let t2 = (t1.0 + ms_to_ns(MAX_OFFSET_MS), 2);

        buffer.insert(t1.0, t1.1);
        buffer.insert(t2.0, t2.1);

        assert_eq!(vec![t1, t2], buffer.iter().collect::<Vec<_>>());
        assert_eq!((0, ms_to_ns(start_ms)), buffer.get_reset_info());

        // This should exceed the maximum allowable timestamp offset,
        // causing the buffer to reset.
        let t3 = (t2.0 + ms_to_ns(MAX_OFFSET_MS + 1), 3);
        buffer.insert(t3.0, t3.1);
        assert_eq!(vec![t3], buffer.iter().collect::<Vec<_>>());
        assert_eq!((1, t3.0), buffer.get_reset_info());
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_enum_off_on(lazy_record: bool) {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let mut recorder = EnumStateRecorder::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();

        recorder.record(SwitchState::OFF);
        recorder.record(SwitchState::ON);
        recorder.record(SwitchState::OFF);
        recorder.record(SwitchState::ON);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                my_switch: {
                    metadata: {
                        name: "my_switch",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    history: {
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
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_multiple_recorders(lazy_record: bool) {
        #[derive(Copy, Clone, Debug, Display, EnumIter, EnumString, Eq, PartialEq, Hash)]
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

        let mut recorder_0 = EnumStateRecorder::new(
            "switch_0".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager.clone()),
                persistence: None,
            },
        )
        .unwrap();
        let mut recorder_1 = EnumStateRecorder::new(
            "switch_1".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();
        recorder_0.record(SwitchState::OFF);
        recorder_0.record(SwitchState::ON);
        recorder_1.record(EnablementState::ENABLED);
        recorder_1.record(EnablementState::DISABLED);

        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                switch_0: {
                    metadata: {
                        name: "switch_0",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                },
               switch_1: {
                    metadata: {
                        name: "switch_1",
                        type: "enum",
                        states: {
                            "DISABLED": 0u64,
                            "ENABLED": 1u64,
                        }
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "ENABLED",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "DISABLED",
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        })
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_enum_three_states(lazy_record: bool) {
        #[derive(Copy, Clone, Debug, Display, EnumIter, EnumString, Eq, PartialEq, Hash)]
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

        let mut recorder = EnumStateRecorder::new(
            "the_best_fan".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();

        recorder.record(FanSpeed::OFF);
        recorder.record(FanSpeed::LOW);
        recorder.record(FanSpeed::HIGH);
        recorder.record(FanSpeed::OFF);
        recorder.record(FanSpeed::HIGH);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                the_best_fan: {
                    metadata: {
                        name: "the_best_fan",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "LOW": 1u64,
                            "HIGH": 2u64,
                        }
                    },
                    history: {
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
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_name_reuse_not_allowed(lazy_record: bool) {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        let recorder = EnumStateRecorder::<SwitchState>::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager.clone()),
                persistence: None,
            },
        )
        .unwrap();

        // While `recorder` is still in scope, its name cannot be reused.
        let result = EnumStateRecorder::<SwitchState>::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager.clone()),
                persistence: None,
            },
        );
        assert!(result.is_err());

        // After `recorder` is dropped, its name can be used again.
        drop(recorder);
        let result = EnumStateRecorder::<SwitchState>::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager.clone()),
                persistence: None,
            },
        );
        assert!(result.is_ok());
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_singleton_manager(lazy_record: bool) {
        let mut recorder = EnumStateRecorder::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions { lazy_record, capacity: 10, manager: None, persistence: None },
        )
        .unwrap();

        recorder.record(SwitchState::OFF);
        recorder.record(SwitchState::ON);
        assert_data_tree!(inspect::component::inspector(), root: {
            power_observability_state_recorders: {
                my_switch: {
                    metadata: {
                        name: "my_switch",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    async fn test_recorder_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<EnumStateRecorder<SwitchState>>();
    }

    async fn test_uint_numeric_type<T: RecordableNumericType>(lazy_record: bool)
    where
        T: Into<u64> + From<u8>,
    {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);
        let mut recorder = NumericStateRecorder::new(
            "my_stateful_thing".into(),
            c"power_test",
            units!(Percent),
            Some((T::from(0), T::from(255))),
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();

        recorder.record(T::from(10));
        recorder.record(T::from(0));
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                my_stateful_thing: {
                    metadata: {
                        name: "my_stateful_thing",
                        type: "numeric",
                        units: "%",
                        range: {
                            min_inc: 0u64,
                            max_inc: 255u64
                        },
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": 10u64,
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": 0u64,
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_uint_numeric_types(lazy_record: bool) {
        test_uint_numeric_type::<u8>(lazy_record).await;
        test_uint_numeric_type::<u16>(lazy_record).await;
        test_uint_numeric_type::<u32>(lazy_record).await;
        test_uint_numeric_type::<u64>(lazy_record).await;
    }

    async fn test_int_numeric_type<T: RecordableNumericType>(lazy_record: bool)
    where
        T: Into<i64> + From<i8>,
    {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);
        let mut recorder = NumericStateRecorder::new(
            "my_stateful_thing".into(),
            c"power_test",
            units!(Number),
            Some((T::from(-128), T::from(127))),
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();

        recorder.record(T::from(10));
        recorder.record(T::from(0));
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                my_stateful_thing: {
                    metadata: {
                        name: "my_stateful_thing",
                        type: "numeric",
                        units: "#",
                        range: {
                            min_inc: -128i64,
                            max_inc: 127i64
                        },
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": 10i64,
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": 0i64,
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_int_numeric_types(lazy_record: bool) {
        test_int_numeric_type::<i8>(lazy_record).await;
        test_int_numeric_type::<i16>(lazy_record).await;
        test_int_numeric_type::<i32>(lazy_record).await;
        test_int_numeric_type::<i64>(lazy_record).await;
    }

    async fn test_float_numeric_type<T: RecordableNumericType>(lazy_record: bool)
    where
        T: Into<f64> + From<u8>,
    {
        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);
        let mut recorder = NumericStateRecorder::new(
            "my_stateful_thing".into(),
            c"power_test",
            units!(Kilo, Hertz),
            Some((T::from(0), T::from(255))),
            RecorderOptions {
                lazy_record,
                capacity: 10,
                manager: Some(manager),
                persistence: None,
            },
        )
        .unwrap();

        recorder.record(T::from(10));
        recorder.record(T::from(0));
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                my_stateful_thing: {
                    metadata: {
                        name: "my_stateful_thing",
                        type: "numeric",
                        units: "kHz",
                        range: {
                            min_inc: 0.0,
                            max_inc: 255.0
                        },
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": 10.0,
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": 0.0,
                        },
                    },
                    reset_info: {
                        count: 0,
                        last_reset_ns: AnyIntProperty,
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_float_numeric_types(lazy_record: bool) {
        test_float_numeric_type::<f32>(lazy_record).await;
        test_float_numeric_type::<f64>(lazy_record).await;
    }

    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_persistence_crash_recovery(lazy_record: bool) {
        use std::fs;
        use tempfile::tempdir;

        // 1. Setup isolated environment
        let dir = tempdir().unwrap();
        let storage_path = dir.path().join("data");
        let volatile_path = dir.path().join("tmp");
        fs::create_dir(&storage_path).unwrap();
        fs::create_dir(&volatile_path).unwrap();

        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        // Helper to generate options
        let create_options = |manager_ref| RecorderOptions {
            lazy_record, // Passed from test_case argument
            capacity: 10,
            manager: Some(manager_ref),
            persistence: Some(
                PersistenceOptions::new("crash_test".to_string())
                    .storage_dir(storage_path.to_str().unwrap())
                    .volatile_dir(volatile_path.to_str().unwrap()),
            ),
        };

        // 2. START RECORDER 1 (Fill data)
        {
            let mut recorder = EnumStateRecorder::<SwitchState>::new(
                "crash_test".into(),
                c"power_test",
                create_options(manager.clone()),
            )
            .unwrap();

            recorder.record(SwitchState::ON);
            recorder.record(SwitchState::OFF);

            // Scope ends, data is persisted to disk
        }

        // Verify disk content
        let curr_csv = storage_path.join("crash_test.csv");
        let content = fs::read_to_string(curr_csv).unwrap();
        assert!(content.contains("ON"));
        assert!(content.contains("OFF"));

        // 3. FORCE "CRASH" STATE
        // Create 'Previous' file so library thinks this is a crash restart, not a reboot.
        // This forces it to READ from storage_path without overwriting it.
        let prev_csv = volatile_path.join("crash_test.csv");
        fs::write(&prev_csv, "").unwrap();

        // 4. START RECORDER 2 (Simulate Restart)
        // This triggers hydration from disk into (Lazy: RingBuffer) or (Eager: BoundedListNode)
        let mut recorder_restarted = EnumStateRecorder::<SwitchState>::new(
            "crash_test".into(),
            c"power_test",
            create_options(manager),
        )
        .unwrap();

        // ASSERTIONS
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                crash_test: {
                    metadata: {
                        name: "crash_test",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                    },
                    reset_info: {
                        count: 0i64, // Matches both lazy (casted i64) and eager (0 literal)
                        last_reset_ns: AnyIntProperty,
                    },
                }
            }
        });

        // 5. RECORD NEW DATA
        recorder_restarted.record(SwitchState::ON);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                crash_test: {
                    metadata: {
                        name: "crash_test",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    history: {
                        "0": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                        "1": {
                            "@time": AnyIntProperty,
                            "value": "OFF",
                        },
                        "2": {
                            "@time": AnyIntProperty,
                            "value": "ON",
                        },
                    },
                    reset_info: {
                        count: 0i64,
                        last_reset_ns: AnyIntProperty,
                    },
                }
            }
        });
    }

    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_persistence_reboot(lazy_record: bool) {
        use std::fs;
        use tempfile::tempdir;

        // 1. Setup isolated environment
        let dir = tempdir().unwrap();
        let storage_path = dir.path().join("data");
        let volatile_path = dir.path().join("tmp");
        fs::create_dir(&storage_path).unwrap();
        fs::create_dir(&volatile_path).unwrap();

        let inspector = Inspector::default();
        let manager = StateRecorderManager::new(&inspector);

        // Helper to generate options pointing to our temp dirs
        let create_options = |manager_ref| RecorderOptions {
            lazy_record,
            capacity: 10,
            manager: Some(manager_ref),
            persistence: Some(
                PersistenceOptions::new("reboot_test".to_string())
                    .storage_dir(storage_path.to_str().unwrap())
                    .volatile_dir(volatile_path.to_str().unwrap()),
            ),
        };

        // 2. SIMULATE FRESH REBOOT STATE
        // - "Current" file exists in persistent storage (saved from previous run).
        // - "Previous" file in volatile storage is MISSING (cleared by OS reboot).
        let curr_csv = storage_path.join("reboot_test.csv");
        // Write raw CSV data simulating timestamps 1000 and 2000
        fs::write(&curr_csv, "1000,ON\n2000,OFF\n").unwrap();

        // Ensure volatile file doesn't exist (simulating clean /tmp)
        let prev_csv = volatile_path.join("reboot_test.csv");
        assert!(!prev_csv.exists());

        // 3. START RECORDER (Trigger Logic)
        let mut recorder = EnumStateRecorder::<SwitchState>::new(
            "reboot_test".into(),
            c"power_test",
            create_options(manager),
        )
        .unwrap();

        // 4. VERIFY FILESYSTEM (Rotation)
        // The file should have been moved from 'data' to 'tmp'.
        assert!(prev_csv.exists(), "Library should have rotated curr -> prev");
        assert!(curr_csv.exists(), "Library should have create a new current file");

        let rotated_content = fs::read_to_string(&prev_csv).unwrap();
        assert!(rotated_content.contains("1000,ON"));
        assert!(rotated_content.contains("2000,OFF"));

        // 5. ASSERTIONS (Inspect)
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                reboot_test: {
                    metadata: {
                        name: "reboot_test",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    // DATA FROM FILE IS HERE (Read Only / Static)
                    previous_boot_history: {
                        "0": {
                            "@time": 1000i64,
                            "value": "ON",
                        },
                        "1": {
                            "@time": 2000i64,
                            "value": "OFF",
                        },
                    },
                    // ACTIVE HISTORY IS EMPTY (Fresh start)
                    history: {},
                    reset_info: {
                        count: 0i64,
                        last_reset_ns: AnyIntProperty,
                    },
                }
            }
        });

        // 6. RECORD NEW DATA AFTER REBOOT
        recorder.record(SwitchState::ON);
        recorder.record(SwitchState::OFF);
        assert_data_tree!(inspector, root: {
            power_observability_state_recorders: {
                reboot_test: {
                    metadata: {
                        name: "reboot_test",
                        type: "enum",
                        states: {
                            "OFF": 0u64,
                            "ON": 1u64,
                        }
                    },
                    // DATA FROM FILE IS HERE (Read Only / Static)
                    previous_boot_history: {
                        "0": {
                            "@time": 1000i64,
                            "value": "ON",
                        },
                        "1": {
                            "@time": 2000i64,
                            "value": "OFF",
                        },
                    },
                    // ACTIVE HISTORY IS NOW POPULATED WITH NEW DATA
                    history: {
                        "0": {
                            "@time": AnyIntProperty, // New timestamp
                            "value": "ON",
                        },
                        "1": {
                            "@time": AnyIntProperty, // New timestamp
                            "value": "OFF",
                        },
                    },
                    reset_info: {
                        count: 0i64,
                        last_reset_ns: AnyIntProperty,
                    },
                }
            }
        });
    }
}
