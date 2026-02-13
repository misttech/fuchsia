// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FrameworkError;

/// A flexible FIDL result.
#[derive(Clone, Debug)]
pub enum FlexibleResult<T, E> {
    /// The value of the flexible call when successful.
    Ok(T),
    /// The error returned from a successful flexible call.
    Err(E),
    /// The error indicating that the flexible call failed.
    FrameworkErr(FrameworkError),
}

impl<T, E> FlexibleResult<T, E> {
    /// Returns whether the flexible result is `Ok`.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Returns whether the flexible result if `Err`.
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    /// Returns whether the flexible result is `FrameworkErr`.
    pub fn is_framework_err(&self) -> bool {
        matches!(self, Self::FrameworkErr(_))
    }

    /// Returns the `Ok` value of the result, if any.
    pub fn ok(self) -> Option<T> {
        if let Self::Ok(value) = self { Some(value) } else { None }
    }

    /// Returns the `Err` value of the result, if any.
    pub fn err(self) -> Option<E> {
        if let Self::Err(error) = self { Some(error) } else { None }
    }

    /// Returns the `FrameworkErr` value of the result, if any.
    pub fn framework_err(self) -> Option<FrameworkError> {
        if let Self::FrameworkErr(error) = self { Some(error) } else { None }
    }

    /// Returns the contained `Ok` value.
    ///
    /// Panics if the result was not `Ok`.
    pub fn unwrap(self) -> T {
        self.ok().unwrap()
    }

    /// Returns the contained `Err` value.
    ///
    /// Panics if the result was not `Err`.
    pub fn unwrap_err(self) -> E {
        self.err().unwrap()
    }

    /// Returns the contained `FrameworkErr` value.
    ///
    /// Panics if the result was not `FrameworkErr`.
    pub fn unwrap_framework_err(self) -> FrameworkError {
        self.framework_err().unwrap()
    }

    /// Converts from `FlexibleResult<T, E>` to `FlexibleResult<&T, &E>`.
    pub fn as_ref(&self) -> FlexibleResult<&T, &E> {
        match self {
            Self::Ok(value) => FlexibleResult::Ok(value),
            Self::Err(error) => FlexibleResult::Err(error),
            Self::FrameworkErr(framework_error) => FlexibleResult::FrameworkErr(*framework_error),
        }
    }
}

impl<T, E, T2, E2> PartialEq<FlexibleResult<T2, E2>> for FlexibleResult<T, E>
where
    T: PartialEq<T2>,
    E: PartialEq<E2>,
{
    fn eq(&self, other: &FlexibleResult<T2, E2>) -> bool {
        match (self, other) {
            (FlexibleResult::Ok(lhs), FlexibleResult::Ok(rhs)) => lhs == rhs,
            (FlexibleResult::Err(lhs), FlexibleResult::Err(rhs)) => lhs == rhs,
            (FlexibleResult::FrameworkErr(lhs), FlexibleResult::FrameworkErr(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
