// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestReporter_RunSuccess(t *testing.T) {
	outDir := t.TempDir()
	reporter := NewReporter(t.TempDir(), outDir, Config{})

	projects := []*pipeline.Project{
		{
			RootPath: "third_party/foo",
		},
	}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := reporter.Run(ctx, projects, nil)
	if err != nil {
		t.Fatalf("Expected successful run, got error: %v", err)
	}
}
