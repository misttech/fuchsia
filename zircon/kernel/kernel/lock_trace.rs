// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Lock contention tracing helpers, emitting events in the `kernel:contention` category.

/// Emits a `kernel:contention` duration event covering the rest of the enclosing scope.
///
/// Must be used in statement position: the scope guard is bound to a hidden local that is dropped,
/// completing the duration event, at the end of the enclosing block.
///
/// # Arguments:
/// - label: Label for the event. Expects a string literal or expression.
/// - ...: List of key => value argument pairs.
#[macro_export]
macro_rules! lock_trace_duration {
    ($label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        let _lock_trace_duration =
            $crate::ktrace_rs::begin_scope!("kernel:contention", $label $(, $key => $val)*);
    };
}
pub use lock_trace_duration;

/// Emits a `kernel:contention` duration begin event.
#[macro_export]
macro_rules! lock_trace_duration_begin {
    ($label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        $crate::ktrace_rs::duration_begin!("kernel:contention", $label $(, $key => $val)*)
    };
}
pub use lock_trace_duration_begin;

/// Emits a `kernel:contention` duration end event.
#[macro_export]
macro_rules! lock_trace_duration_end {
    ($label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        $crate::ktrace_rs::duration_end!("kernel:contention", $label $(, $key => $val)*)
    };
}
pub use lock_trace_duration_end;

/// Emits a `kernel:contention` flow begin event associated with the current thread.
#[macro_export]
macro_rules! lock_trace_flow_begin {
    ($label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        $crate::ktrace_rs::flow_begin!("kernel:contention", $label, $flow_id $(, $key => $val)*)
    };
}
pub use lock_trace_flow_begin;

/// Emits a `kernel:contention` flow step event associated with the current thread.
#[macro_export]
macro_rules! lock_trace_flow_step {
    ($label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        $crate::ktrace_rs::flow_step!("kernel:contention", $label, $flow_id $(, $key => $val)*)
    };
}
pub use lock_trace_flow_step;

/// Emits a `kernel:contention` flow end event associated with the current thread.
#[macro_export]
macro_rules! lock_trace_flow_end {
    ($label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        $crate::ktrace_rs::flow_end!("kernel:contention", $label, $flow_id $(, $key => $val)*)
    };
}
pub use lock_trace_flow_end;
