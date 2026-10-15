// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testsharder

import (
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/build"
)

func TestTestDurationsMap(t *testing.T) {
	m := NewTestDurationsMap([]build.TestDuration{
		{
			Name:           defaultDurationKey,
			MedianDuration: 1,
		},
		{
			Name:           "foo",
			MedianDuration: 2,
		},
		{
			Name:           "bar",
			MedianDuration: 3,
		},
		{
			Name:           "bar-core.x64",
			MedianDuration: 4,
		},
		{
			Name:           "baz",
			MedianDuration: 5,
		},
		// Later entries overwrite earlier entries with the same name (e.g. when
		// builder-specific durations override default durations).
		{
			Name:           "baz",
			MedianDuration: 6,
		},
	})

	assertDurationEquals := func(test Test, expected time.Duration) {
		t.Helper()
		duration := m.Get(test)
		if duration.MedianDuration != expected {
			t.Fatalf("wrong duration for test %+v: got %s, want %s", test, duration.MedianDuration, expected)
		}
	}

	assertDurationEquals(Test{Test: build.Test{Name: "unknown-test"}}, 1)
	assertDurationEquals(Test{Test: build.Test{Name: "unknown-test-core.x64"}, ProductBundle: "core.x64"}, 1)
	assertDurationEquals(Test{Test: build.Test{Name: "foo"}}, 2)
	assertDurationEquals(Test{Test: build.Test{Name: "foo-core.x64"}, ProductBundle: "core.x64"}, 2)
	assertDurationEquals(Test{Test: build.Test{Name: "bar"}}, 3)
	assertDurationEquals(Test{Test: build.Test{Name: "bar-core.x64"}, ProductBundle: "core.x64"}, 4)
	assertDurationEquals(Test{Test: build.Test{Name: "baz"}}, 6)
}
