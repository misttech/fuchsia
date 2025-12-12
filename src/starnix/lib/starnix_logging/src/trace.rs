// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use fuchsia_trace::Scope as TraceScope;

// This needs to be available to the macros in this module without clients having to depend on
// fuchsia_trace themselves.
#[doc(hidden)]
pub use fuchsia_trace as __fuchsia_trace;

// The trace category used for starnix-related traces.
pub const CATEGORY_STARNIX: &'static str = "starnix";

// The trace category used for memory manager related traces.
pub const CATEGORY_STARNIX_MM: &'static str = "starnix:mm";

// The trace category used for security related traces.
pub const CATEGORY_STARNIX_SECURITY: &'static str = "starnix:security";

// The name used to track the duration in Starnix while executing a task.
pub const NAME_RUN_TASK: &'static str = "RunTask";

// The trace category used for atrace events generated within starnix.
pub const CATEGORY_ATRACE: &'static str = "starnix:atrace";

// The trace category used for trace events about emitting trace events.
pub const CATEGORY_TRACE_META: &'static str = "trace_meta";

// The name used to identify blob records from the container's Perfetto daemon.
pub const NAME_PERFETTO_BLOB: &'static str = "starnix_perfetto";

// The name used to track the duration of creating a container.
pub const NAME_CREATE_CONTAINER: &'static str = "CreateContainer";

// The name used to track the start time of the starnix kernel.
pub const NAME_START_KERNEL: &'static str = "StartKernel";

// The name used to track when a thread was kicked.
pub const NAME_RESTRICTED_KICK: &'static str = "RestrictedKick";

// The name used to track the duration for inline exception handling.
pub const NAME_HANDLE_EXCEPTION: &'static str = "HandleException";

// The names used to track durations for restricted state I/O.
pub const NAME_READ_RESTRICTED_STATE: &'static str = "ReadRestrictedState";
pub const NAME_WRITE_RESTRICTED_STATE: &'static str = "WriteRestrictedState";

// The name used to track the duration of checking whether the task loop should exit.
pub const NAME_CHECK_TASK_EXIT: &'static str = "CheckTaskExit";

pub const ARG_NAME: &'static str = "name";

#[inline]
pub fn regular_trace_category_enabled(category: &'static str) -> bool {
    fuchsia_trace::category_enabled(category)
}

#[macro_export]
macro_rules! trace_instant {
    ($category:expr, $name:expr, $scope:expr $(, $key:expr => $val:expr)*) => {
        $crate::__fuchsia_trace::instant!($category, $name, $scope $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! firehose_trace_instant {
    ($category:expr, $name:expr, $scope:expr $(, $key:expr => $val:expr)*) => {
        $crate::trace_instant!($category, $name, $scope $(, $key => $val)*);
    }
}

// The `trace_duration` macro defines a `_scope` instead of executing a statement because the
// lifetime of the `_scope` variable corresponds to the duration.
#[macro_export]
macro_rules! trace_duration {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        let args;
        let _scope = {
            static CACHE: $crate::__fuchsia_trace::trace_site_t = $crate::__fuchsia_trace::trace_site_t::new(0);
            if let Some(_context) =
                    $crate::__fuchsia_trace::TraceCategoryContext::acquire_cached($category, &CACHE) {
                args = [$($crate::__fuchsia_trace::ArgValue::of($key, $val)),*];
                Some($crate::__fuchsia_trace::duration($category, $name, &args))
            } else {
                None
            }
        };
    }
}

#[macro_export]
macro_rules! firehose_trace_duration {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        $crate::trace_duration!($category, $name $(, $key => $val)*);
    }
}

#[macro_export]
macro_rules! trace_duration_begin {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        $crate::__fuchsia_trace::duration_begin!($category, $name $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! firehose_trace_duration_begin {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        $crate::trace_duration_begin!($category, $name $(, $key => $val)*);
    }
}

#[macro_export]
macro_rules! trace_duration_end {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        $crate::__fuchsia_trace::duration_end!($category, $name $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! firehose_trace_duration_end {
    ($category:expr, $name:expr $(, $key:expr => $val:expr)*) => {
        $crate::trace_duration_end!($category, $name $(, $key => $val)*);
    }
}

#[macro_export]
macro_rules! trace_flow_begin {
    ($category:expr, $name:expr, $flow_id:expr $(, $key:expr => $val:expr)*) => {
        let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
        $crate::__fuchsia_trace::flow_begin!($category, $name, _flow_id $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! trace_flow_step {
    ($category:expr, $name:expr, $flow_id:expr $(, $key:expr => $val:expr)*) => {
        let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
        $crate::__fuchsia_trace::flow_step!($category, $name, _flow_id $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! trace_flow_end {
    ($category:expr, $name:expr, $flow_id:expr $(, $key:expr => $val:expr)*) => {
        let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
        $crate::__fuchsia_trace::flow_end!($category, $name, _flow_id $(, $key => $val)*);
    };
}

#[macro_export]
macro_rules! trace_instaflow_begin {
    (
        $category:expr,
        $flow_name:expr,
        $step_name:expr,
        $flow_id:expr
        $(, $key:expr => $val:expr)*
    ) => {
        {
            let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
            $crate::__fuchsia_trace::instaflow_begin!(
                $category,
                $flow_name,
                $step_name,
                _flow_id
                $(, $key => $val)*
            );
        }
    };
}

#[macro_export]
macro_rules! trace_instaflow_end {
    (
        $category:expr,
        $flow_name:expr,
        $step_name:expr,
        $flow_id:expr
        $(, $key:expr => $val:expr)*
    ) => {
        {
            let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
            $crate::__fuchsia_trace::instaflow_end!(
                $category,
                $flow_name,
                $step_name,
                _flow_id
                $(, $key => $val)*
            );
        }
    };
}

#[macro_export]
macro_rules! trace_instaflow_step {
    (
        $category:expr,
        $flow_name:expr,
        $step_name:expr,
        $flow_id:expr
        $(, $key:expr => $val:expr)*
    ) => {
        {
            let _flow_id: $crate::__fuchsia_trace::Id = $flow_id;
            $crate::__fuchsia_trace::instaflow_step!(
                $category,
                $flow_name,
                $step_name,
                _flow_id
                $(, $key => $val)*
            );
        }
    };
}
