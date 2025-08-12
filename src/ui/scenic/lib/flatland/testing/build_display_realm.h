// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_TESTING_BUILD_DISPLAY_REALM_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_TESTING_BUILD_DISPLAY_REALM_H_

#include <lib/async/dispatcher.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>

namespace flatland::testing {

// Returns a realm that serves the `fuchsia.hardware.display.Service` service
// provided by the component to the test.
//
// `dispatcher` must be non-null outlive the lifetime of the constructed
// `RealmRoot`.
component_testing::RealmRoot BuildDisplayRealm(async_dispatcher_t* dispatcher);

// Returns a realm that serves the `fuchsia.hardware.display.Service` service
// provided by the fake display stack to the test.
//
// `dispatcher` must be non-null outlive the lifetime of the constructed
// `RealmRoot`.
component_testing::RealmRoot BuildFakeDisplayRealm(async_dispatcher_t* dispatcher);

}  // namespace flatland::testing

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_TESTING_BUILD_DISPLAY_REALM_H_
