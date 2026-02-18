// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fint

import (
	"os"
	"path/filepath"
	"slices"
	"testing"

	fintpb "go.fuchsia.dev/fuchsia/tools/integration/fint/proto"
)

func TestNewRunner(t *testing.T) {
	t.Run("sets PYTHONPYCACHEPREFIX when build dir is set", func(t *testing.T) {
		buildDir := "/path/to/build"
		contextSpec := &fintpb.Context{
			BuildDir: buildDir,
		}
		// Ensure environment variable is not set during test.
		orig := os.Getenv("PYTHONPYCACHEPREFIX")
		os.Setenv("PYTHONPYCACHEPREFIX", "")
		defer os.Setenv("PYTHONPYCACHEPREFIX", orig)

		runner := newRunner(contextSpec)
		wantEnv := "PYTHONPYCACHEPREFIX=" + filepath.Join(buildDir, "__pycache__")
		if !slices.Contains(runner.Env, wantEnv) {
			t.Errorf("Runner environment %v does not contain %q", runner.Env, wantEnv)
		}
	})

	t.Run("honors existing PYTHONPYCACHEPREFIX", func(t *testing.T) {
		buildDir := "/path/to/build"
		contextSpec := &fintpb.Context{
			BuildDir: buildDir,
		}
		orig := os.Getenv("PYTHONPYCACHEPREFIX")
		wantEnv := "/some/other/path"
		os.Setenv("PYTHONPYCACHEPREFIX", wantEnv)
		defer os.Setenv("PYTHONPYCACHEPREFIX", orig)

		runner := newRunner(contextSpec)
		if slices.Contains(runner.Env, "PYTHONPYCACHEPREFIX=") {
			t.Errorf("Runner environment %v should not contain PYTHONPYCACHEPREFIX when it's already set in environment", runner.Env)
		}
	})

	t.Run("does not set PYTHONPYCACHEPREFIX when build dir is empty", func(t *testing.T) {
		contextSpec := &fintpb.Context{
			BuildDir: "",
		}
		orig := os.Getenv("PYTHONPYCACHEPREFIX")
		os.Setenv("PYTHONPYCACHEPREFIX", "")
		defer os.Setenv("PYTHONPYCACHEPREFIX", orig)

		runner := newRunner(contextSpec)
		if slices.Contains(runner.Env, "PYTHONPYCACHEPREFIX=") {
			t.Errorf("Runner environment %v should not contain PYTHONPYCACHEPREFIX when build dir is empty", runner.Env)
		}
	})

	t.Run("handles nil context spec", func(t *testing.T) {
		runner := newRunner(nil)
		if slices.Contains(runner.Env, "PYTHONPYCACHEPREFIX=") {
			t.Errorf("Runner environment %v should not contain PYTHONPYCACHEPREFIX when context spec is nil", runner.Env)
		}
	})
}
