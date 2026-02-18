// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/testutil"
)

func TestGolibReadmeGeneration(t *testing.T) {
	tests := []struct {
		name string
		root string
	}{
		{
			name: "github.com/foo/bar",
			root: "third_party/golibs/vendor/github.com/foo/bar",
		},
		{
			name: "golang.org/foo/bar",
			root: "third_party/golibs/vendor/golang.org/foo/bar",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			tempDir := t.TempDir()
			testutil.DumpTestData(t, testDataFS, tempDir)
			testDataDir := filepath.Join(tempDir, "testdata")
			wantPath := filepath.Join(testDataDir, "go", tt.root, "want.json")

			got, err := NewGolibReadme(filepath.Join(testDataDir, "go", tt.root))
			if err != nil {
				t.Fatalf("%v: expected no error, got %v.", t.Name(), err)
			}

			runReadmeDiffTest(t, wantPath, got)

		})
	}
}
