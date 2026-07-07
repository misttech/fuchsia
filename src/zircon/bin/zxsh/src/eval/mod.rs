// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![allow(unused_imports)]

pub mod arithmetic;
pub mod execution_context;
pub mod expand;
pub mod format;
pub mod glob;
pub mod state;

pub use execution_context::ExecutionContext;
pub use state::{
    RLIM_INFINITY, RLIMIT_CORE, RLIMIT_FSIZE, RLIMIT_NOFILE, ShellEnv, ShellPath, ShellState,
};

/// Represents the outcome of evaluating a shell command or statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    /// Normal command execution completion with an exit status code.
    Code(i32),
    /// Explicit shell exit request (e.g. via `exit` builtin) with a status code.
    Exit(i32),
    /// Return from a shell function or sourced script with a status code.
    Return(i32),
    /// Break out of `N` enclosing loop levels.
    Break(u32),
    /// Continue execution at the next iteration of `N` enclosing loop levels.
    Continue(u32),
}

impl EvalOutcome {
    /// Returns the effective numeric exit status code for this outcome.
    pub fn exit_code(&self) -> i32 {
        match self {
            EvalOutcome::Code(code) | EvalOutcome::Exit(code) | EvalOutcome::Return(code) => *code,
            EvalOutcome::Break(_) | EvalOutcome::Continue(_) => 0,
        }
    }
}
