// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use fuchsia_trace::Scope as TraceScope;

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
pub const CATEGORY_TRACE_META: &'static str = "starnix:trace_meta";

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
pub const NAME_MAP_RESTRICTED_STATE: &'static str = "MapRestrictedState";

// The name used to track the duration of checking whether the task loop should exit.
pub const NAME_CHECK_TASK_EXIT: &'static str = "CheckTaskExit";

pub const ARG_NAME: &'static str = "name";

#[inline]
pub fn regular_trace_category_enabled(category: &'static str) -> bool {
    fuchsia_trace::category_enabled(category)
}

pub use fuchsia_trace::{instant as trace_instant, instant as firehose_trace_instant};

pub use fuchsia_trace::{duration as trace_duration, duration as firehose_trace_duration};

pub use fuchsia_trace::{
    duration_begin as trace_duration_begin, duration_begin as firehose_trace_duration_begin,
};

pub use fuchsia_trace::{
    duration_end as trace_duration_end, duration_end as firehose_trace_duration_end,
};

pub use fuchsia_trace::{
    flow_begin as trace_flow_begin, flow_end as trace_flow_end, flow_step as trace_flow_step,
};

pub use fuchsia_trace::{
    instaflow_begin as trace_instaflow_begin, instaflow_end as trace_instaflow_end,
    instaflow_step as trace_instaflow_step,
};
