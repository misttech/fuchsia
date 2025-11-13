// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(feature = "tracing")]
pub use fuchsia_trace::{
    Id, Scope, TraceFutureExt, duration, flow_begin, flow_end, flow_step, instant,
    trace_future_args,
};

#[cfg(not(feature = "tracing"))]
mod noop;
#[cfg(not(feature = "tracing"))]
pub mod __backend {
    pub use crate::noop::*;
}
#[cfg(not(feature = "tracing"))]
pub use crate::noop::{Id, Scope, TraceFutureExt};

#[cfg(test)]
mod tests {
    use crate::*;

    #[fuchsia::test]
    fn test_duration() {
        let trace_only_var = 6;
        duration!(c"category", c"name");
        duration!(c"category", c"name", "arg" => 5);
        duration!(c"category", c"name", "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_instant() {
        let trace_only_var = 6;
        instant!(c"category", c"name", Scope::Thread);
        instant!(c"category", c"name", Scope::Thread, "arg" => 5);
        instant!(c"category", c"name", Scope::Thread, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_begin() {
        let trace_only_var = 6;
        let flow_id: Id = 5u64.into();
        flow_begin!(c"category", c"name", flow_id);
        flow_begin!(c"category", c"name", flow_id, "arg" => 5);
        flow_begin!(c"category", c"name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_step() {
        let trace_only_var = 6;
        let flow_id: Id = 5u64.into();
        flow_step!(c"category", c"name", flow_id);
        flow_step!(c"category", c"name", flow_id, "arg" => 5);
        flow_step!(c"category", c"name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    fn test_flow_end() {
        let trace_only_var = 6;
        let flow_id: Id = 5u64.into();
        flow_end!(c"category", c"name", flow_id);
        flow_end!(c"category", c"name", flow_id, "arg" => 5);
        flow_end!(c"category", c"name", flow_id, "arg" => 5, "arg2" => trace_only_var);
    }

    #[fuchsia::test]
    async fn test_trace_future() {
        let value = async move { 5 }.trace(trace_future_args!(c"category", c"name")).await;
        assert_eq!(value, 5);

        let value =
            async move { 5 }.trace(trace_future_args!(c"category", c"name", "arg1" => 6)).await;
        assert_eq!(value, 5);

        let trace_only_var = 7;
        let value = async move { 5 }
            .trace(trace_future_args!(c"category", c"name", "arg1" => 6, "ar2" => trace_only_var))
            .await;
        assert_eq!(value, 5);
    }

    #[fuchsia::test]
    fn test_arg_types() {
        duration!(c"category", c"name", "bool" => true);
        duration!(c"category", c"name", "i32" => 5i32, "u32" => 5u32);
        duration!(c"category", c"name", "i64" => 5i64, "u64" => 5u64);
        duration!(c"category", c"name", "isize" => 5isize, "usize" => 5usize);
        duration!(c"category", c"name", "f64" => 5f64);

        let owned_str = "test-str".to_owned();
        duration!(c"category", c"name", "str" => owned_str.as_str());

        let mut value = 5u64;
        duration!(c"category", c"name", "const-ptr" => &value as *const u64);
        duration!(c"category", c"name", "mut-ptr" => &mut value as *mut u64);
    }
}
