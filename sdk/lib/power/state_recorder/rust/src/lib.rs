// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use fuchsia_inspect::Inspector;
use fuchsia_inspect_contrib::nodes::BoundedListNode;
use fuchsia_sync::Mutex;
use futures_util::FutureExt;
use std::cmp::Eq;
pub use std::collections::HashMap;
pub use std::ffi::{CStr, CString};
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::marker::PhantomData;
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
pub trait RecordableEnum:
    Copy + Debug + Display + Eq + Hash + IntoEnumIterator + Into<u64> + Send
{
}
impl<T: Copy + Debug + Display + Eq + Hash + IntoEnumIterator + Into<u64> + Send> RecordableEnum
    for T
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
    _root_node: inspect::Node,
    trace_id: ftrace::Id,
    trace_track_name: &'static CStr,
    trace_state_event: Option<ftrace::AsyncScope>,
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

        let history = if options.lazy_record {
            let history_data =
                Arc::new(Mutex::new(TimestampRingBuffer::<T>::with_capacity(options.capacity)));
            let history_data_cloned = history_data.clone();
            node.record_lazy_child("history", move || {
                let history = history_data_cloned.clone();
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
            RecorderHistory::Lazy(history_data)
        } else {
            RecorderHistory::Eager(BoundedListNode::new(
                node.create_child("history"),
                options.capacity,
            ))
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

impl<T: RecordableEnum> Drop for EnumStateRecorder<T> {
    fn drop(&mut self) {
        self.manager.lock().unregister_name(&self.name);
    }
}

/// To be recordable, a numeric type must, in essence, be able to widen into a trace-compatible
/// type and an Inspect-compatible type. Users are not expected to implement this trait; this
/// module implements it for common numeric types below.
pub trait RecordableNumericType: Copy + Debug + Sized + Send + 'static {
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

/// Options for NumericStateRecorder and EnumStateRecorder
pub struct RecorderOptions {
    // If true, recorder will lazily record values to inspect. Otherwise, will record eagerly.
    pub lazy_record: bool,
    /// Maximum number of recorded values to store on a rolling basis.
    pub capacity: usize,
    /// Optional. If not set, the Recorder will be linked to this module's singleton
    /// StateRecorderManager, which in turn corresponds to the singleton Inspector.
    /// If set, the manager supplied here will be used.
    pub manager: Option<Arc<Mutex<StateRecorderManager>>>,
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

        let history = if options.lazy_record {
            let history_data =
                Arc::new(Mutex::new(TimestampRingBuffer::<T>::with_capacity(options.capacity)));
            let history_data_cloned = history_data.clone();
            node.record_lazy_child("history", move || {
                let history = history_data_cloned.clone();
                async move {
                    let inspector = Inspector::default();
                    let node = inspector.root();
                    for (i, (timestamp, state_value)) in history.lock().iter().enumerate() {
                        node.record_child(format!("{}", i), |node| {
                            node.record_int("@time", timestamp);
                            state_value.record(&node, "value");
                        });
                    }
                    Ok(inspector)
                }
                .boxed()
            });
            RecorderHistory::Lazy(history_data)
        } else {
            RecorderHistory::Eager(BoundedListNode::new(
                node.create_child("history"),
                options.capacity,
            ))
        };

        Ok(Self {
            manager,
            name,
            trace_category,
            trace_name,
            units,
            history,
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
    use strum_macros::{Display, EnumIter};
    use test_case::test_case;

    #[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash)]
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

        // Buffer is already at capacity, so this should overwrite the first element.
        let t4 = (ms_to_ns(start_ms + 4000), 4);
        buffer.insert(t4.0, t4.1);
        assert_eq!(vec![t2, t3, t4], buffer.iter().collect::<Vec<_>>());
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

        // This should exceed the maximum allowable timestamp offset,
        // causing the buffer to reset.
        let t3 = (t2.0 + ms_to_ns(MAX_OFFSET_MS + 1), 3);
        buffer.insert(t3.0, t3.1);
        assert_eq!(vec![t3], buffer.iter().collect::<Vec<_>>());
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
                    }
                }
            }
        });
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_multiple_recorders(lazy_record: bool) {
        #[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash)]
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager.clone()) },
        )
        .unwrap();
        let mut recorder_1 = EnumStateRecorder::new(
            "switch_1".into(),
            c"power_test",
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
                    }
                }
            }
        })
    }

    #[test_case(false; "eager")]
    #[test_case(true; "lazy")]
    #[fuchsia::test]
    async fn test_enum_three_states(lazy_record: bool) {
        #[derive(Copy, Clone, Debug, Display, EnumIter, Eq, PartialEq, Hash)]
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager.clone()) },
        )
        .unwrap();

        // While `recorder` is still in scope, its name cannot be reused.
        let result = EnumStateRecorder::<SwitchState>::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager.clone()) },
        );
        assert!(result.is_err());

        // After `recorder` is dropped, its name can be used again.
        drop(recorder);
        let result = EnumStateRecorder::<SwitchState>::new(
            "my_switch".into(),
            c"power_test",
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager.clone()) },
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
            RecorderOptions { lazy_record, capacity: 10, manager: None },
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
            RecorderOptions { lazy_record, capacity: 10, manager: Some(manager) },
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
}
