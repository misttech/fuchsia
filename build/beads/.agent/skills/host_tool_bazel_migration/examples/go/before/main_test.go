// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestMigration(t *testing.T) {

	if diff := cmp.Diff(helloMigration(), "hello, migration!"); diff != "" {
		t.Errorf("helloMigration() diff (-got +want): %s", diff)
	}
}
