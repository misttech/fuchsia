// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::FrameworkError;

/// A flexible FIDL response.
#[derive(Clone, Debug)]
pub enum Flexible<T> {
    /// The value of the flexible call when successful.
    Ok(T),
    /// The error indicating that the flexible call failed.
    FrameworkErr(FrameworkError),
}

impl<T> Flexible<T> {
    /// Returns whether the flexible response is `Ok`.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Returns whether the flexible response is `FrameworkErr`.
    pub fn is_framework_err(&self) -> bool {
        matches!(self, Self::FrameworkErr(_))
    }

    /// Returns the `Ok` value of the response, if any.
    pub fn ok(self) -> Option<T> {
        if let Self::Ok(value) = self { Some(value) } else { None }
    }

    /// Returns the `FrameworkErr` value of the response, if any.
    pub fn framework_err(self) -> Option<FrameworkError> {
        if let Self::FrameworkErr(error) = self { Some(error) } else { None }
    }

    /// Returns the contained `Ok` value.
    ///
    /// Panics if the response was not `Ok`.
    pub fn unwrap(self) -> T {
        self.ok().unwrap()
    }

    /// Returns the contained `FrameworkErr` value.
    ///
    /// Panics if the response was not `FrameworkErr`.
    pub fn unwrap_framework_err(self) -> FrameworkError {
        self.framework_err().unwrap()
    }

    /// Converts from `&Flexible<T>` to `Flexible<&T>`.
    pub fn as_ref(&self) -> Flexible<&T> {
        match self {
            Self::Ok(value) => Flexible::Ok(value),
            Self::FrameworkErr(framework_error) => Flexible::FrameworkErr(*framework_error),
        }
    }
}

impl<T, T2> PartialEq<Flexible<T2>> for Flexible<T>
where
    T: PartialEq<T2>,
{
    fn eq(&self, other: &Flexible<T2>) -> bool {
        match (self, other) {
            (Flexible::Ok(lhs), Flexible::Ok(rhs)) => lhs == rhs,
            (Flexible::FrameworkErr(lhs), Flexible::FrameworkErr(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}
