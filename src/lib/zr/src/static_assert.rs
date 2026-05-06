// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Compile-time assertion.
/// Fails to compile if the condition is false.
#[macro_export]
macro_rules! static_assert {
    ($x:expr $(,)?) => {
        const _: [(); 0 - !{
            const ASSERT: bool = $x;
            ASSERT
        } as usize] = [];
    };
}
