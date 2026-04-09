// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"testing"
)

// =========================================================================
// Configuration and Initialization Tests (config.go)
// =========================================================================

func TestResultConfig_Merge(t *testing.T) {
	c1 := NewConfig()
	c1.FuchsiaDir = "/original/dir"
	c1.Outputs = []string{"out1.txt"}
	c1.AllowLists = []*AllowList{
		{Name: "List1"},
	}

	c2 := NewConfig()
	c2.FuchsiaDir = "/new/dir"
	c2.Outputs = []string{"out2.txt"}
	c2.AllowLists = []*AllowList{
		{Name: "List2"},
	}

	c1.Merge(c2)

	if c1.FuchsiaDir != "/original/dir" {
		t.Errorf("Expected FuchsiaDir to NOT be overridden if already set, got %q", c1.FuchsiaDir)
	}

	if len(c1.Outputs) != 2 || c1.Outputs[0] != "out1.txt" || c1.Outputs[1] != "out2.txt" {
		t.Errorf("Expected Outputs to be appended, got %v", c1.Outputs)
	}

	if len(c1.AllowLists) != 2 || c1.AllowLists[0].Name != "List1" || c1.AllowLists[1].Name != "List2" {
		t.Errorf("Expected AllowLists to be appended, got %v", c1.AllowLists)
	}
}
