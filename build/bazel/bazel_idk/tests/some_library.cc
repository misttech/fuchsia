// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <dependency.h>
#include <lib/test_lib/test_header.h>

__attribute__((__visibility__("default"))) int SomeFunction() { return RequiredFunction(42); }
