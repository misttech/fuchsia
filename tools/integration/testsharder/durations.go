// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testsharder

import (
	"strings"

	"go.fuchsia.dev/fuchsia/tools/build"
)

// Exactly one entry in the durations file will have this as its test_name field. The
// durations specified by this entry will be applied to any tests that aren't
// found in the durations file (e.g. because they have recently been added or
// renamed).
var defaultDurationKey = "*"

// TestDurationsMap maps test names to corresponding test duration data.
type TestDurationsMap map[string]build.TestDuration

func NewTestDurationsMap(durations []build.TestDuration) TestDurationsMap {
	durationsMap := TestDurationsMap{}
	for _, d := range durations {
		durationsMap[d.Name] = d
	}
	return durationsMap
}

// Get returns the duration data for a given test. If the test is not included
// in the durations map, it falls back to looking up the base test name without
// the "-<product_bundle>" suffix (if applicable) before returning the default
// duration data.
func (m TestDurationsMap) Get(t Test) build.TestDuration {
	if testData, ok := m[t.Test.Name]; ok {
		return testData
	}
	if t.ProductBundle != "" {
		baseName := strings.TrimSuffix(t.Test.Name, "-"+t.ProductBundle)
		if testData, ok := m[baseName]; ok {
			return testData
		}
	}
	return m[defaultDurationKey]
}
