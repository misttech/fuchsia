// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package cli

import (
	"context"
	"flag"
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/artifacts"
)

func TestRepeatableBuildVarParsing(t *testing.T) {
	for _, tc := range []struct {
		name            string
		kind            repeatableBuildKind
		input           string
		expectedValue   string
		expectedVersion string
		expectError     bool
	}{
		{
			name:            "build id plain",
			kind:            buildIdKind,
			input:           "12345",
			expectedValue:   "12345",
			expectedVersion: "latest",
		},
		{
			name:            "build id with latest",
			kind:            buildIdKind,
			input:           "12345:latest",
			expectedValue:   "12345",
			expectedVersion: "latest",
		},
		{
			name:            "build id with fromApiLevel",
			kind:            buildIdKind,
			input:           "12345:fromApiLevel",
			expectedValue:   "12345",
			expectedVersion: "fromApiLevel",
		},
		{
			name:          "builder name",
			kind:          builderNameKind,
			input:         "core.x64-release",
			expectedValue: "core.x64-release",
		},
		{
			name:          "fuchsia build dir",
			kind:          fuchsiaBuildDirKind,
			input:         "/path/to/out/default",
			expectedValue: "/path/to/out/default",
		},
		{
			name:          "product bundle dir",
			kind:          productBundleDirKind,
			input:         "/path/to/pb",
			expectedValue: "/path/to/pb",
		},
		{
			name:        "empty string error",
			kind:        buildIdKind,
			input:       "",
			expectError: true,
		},
		{
			name:        "invalid version policy error",
			kind:        buildIdKind,
			input:       "12345:invalid_policy",
			expectError: true,
		},
		{
			name:        "blob fetch mode not allowed in version specifier",
			kind:        buildIdKind,
			input:       "12345:lazy",
			expectError: true,
		},
		{
			name:        "blob fetch mode prefetch not allowed in version specifier",
			kind:        buildIdKind,
			input:       "12345:prefetch",
			expectError: true,
		},
		{
			name:        "too many parts in specifier error",
			kind:        buildIdKind,
			input:       "12345:latest:lazy",
			expectError: true,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := &RepeatableBuildConfig{}
			rbVar := repeatableBuildVar{c: c, kind: tc.kind}
			err := rbVar.Set(tc.input)
			if tc.expectError {
				if err == nil {
					t.Fatalf("expected error for input %q, got nil", tc.input)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error for input %q: %v", tc.input, err)
			}
			if len(c.builds) != 1 {
				t.Fatalf("expected 1 build, got %d", len(c.builds))
			}
			b := c.builds[0]
			if b.kind != tc.kind {
				t.Fatalf("expected kind %v, got %v", tc.kind, b.kind)
			}
			if b.value != tc.expectedValue {
				t.Fatalf("expected value %q, got %q", tc.expectedValue, b.value)
			}
			if tc.expectedVersion != "" && b.versionPolicy != tc.expectedVersion {
				t.Fatalf("expected versionPolicy %q, got %q", tc.expectedVersion, b.versionPolicy)
			}
		})
	}
}

func TestRepeatableBuildConfigBlobFetchModeDefault(t *testing.T) {
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	c := NewRepeatableBuildConfig(fs, nil, nil, "", "")

	args := []string{
		"-fuchsia-build-dir", "/tmp/dir1",
		"-product-bundle-dir", "/tmp/dir2",
	}
	if err := fs.Parse(args); err != nil {
		t.Fatalf("failed to parse flags: %v", err)
	}

	if c.blobFetchMode != artifacts.Unspecified {
		t.Fatalf("expected default blobFetchMode Unspecified, got %v", c.blobFetchMode)
	}

	builds, err := c.GetBuilds(context.Background(), nil, "")
	if err != nil {
		t.Fatalf("GetBuilds failed: %v", err)
	}

	if len(builds) != 2 {
		t.Fatalf("expected 2 builds, got %d", len(builds))
	}

	for i, b := range builds {
		if b.BlobFetchMode != artifacts.Unspecified {
			t.Fatalf("build %d: expected Unspecified, got %v", i, b.BlobFetchMode)
		}
	}
}

func TestRepeatableBuildConfigBlobFetchModeConfigured(t *testing.T) {
	for _, modeStr := range []string{"lazy", "prefetch"} {
		t.Run(modeStr, func(t *testing.T) {
			fs := flag.NewFlagSet("test", flag.ContinueOnError)
			c := NewRepeatableBuildConfig(fs, nil, nil, "", "")

			args := []string{
				"-blob-fetch-mode", modeStr,
				"-fuchsia-build-dir", "/tmp/dir1",
			}
			if err := fs.Parse(args); err != nil {
				t.Fatalf("failed to parse flags: %v", err)
			}

			expectedMode, err := artifacts.ParseBlobFetchMode(modeStr)
			if err != nil {
				t.Fatalf("failed to parse mode: %v", err)
			}

			if c.blobFetchMode != expectedMode {
				t.Fatalf("expected blobFetchMode %v, got %v", expectedMode, c.blobFetchMode)
			}

			builds, err := c.GetBuilds(context.Background(), nil, "")
			if err != nil {
				t.Fatalf("GetBuilds failed: %v", err)
			}

			if len(builds) != 1 {
				t.Fatalf("expected 1 build, got %d", len(builds))
			}

			if builds[0].BlobFetchMode != expectedMode {
				t.Fatalf("expected %v, got %v", expectedMode, builds[0].BlobFetchMode)
			}
			if builds[0].ProductBundle == nil {
				t.Fatalf("expected ProductBundle to be populated, got nil")
			}
		})
	}
}

func TestRepeatableBuildConfigProductBundleName(t *testing.T) {
	ctx := context.Background()
	tmpDir := t.TempDir()
	pbJSON := `[
		{"name": "core.arm64", "path": "obj/build/images/fuchsia/product_bundle"},
		{"name": "workbench_eng.arm64", "path": "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle"}
	]`
	if err := os.WriteFile(filepath.Join(tmpDir, "product_bundles.json"), []byte(pbJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	// Multiple product bundles without -product-bundle-name should fail.
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	c := NewRepeatableBuildConfig(fs, nil, nil, "", "")
	if err := fs.Parse([]string{"-fuchsia-build-dir", tmpDir}); err != nil {
		t.Fatalf("failed to parse flags: %v", err)
	}
	if _, err := c.GetBuilds(ctx, nil, ""); err == nil {
		t.Fatal("expected error for multiple product bundles without -product-bundle-name, got nil")
	}

	// Specifying -product-bundle-name resolves the matching ProductBundle.
	fs = flag.NewFlagSet("test", flag.ContinueOnError)
	c = NewRepeatableBuildConfig(fs, nil, nil, "", "")
	if err := fs.Parse([]string{
		"-fuchsia-build-dir", tmpDir,
		"-product-bundle-name", "workbench_eng.arm64",
	}); err != nil {
		t.Fatalf("failed to parse flags: %v", err)
	}
	builds, err := c.GetBuilds(ctx, nil, "")
	if err != nil {
		t.Fatalf("GetBuilds failed: %v", err)
	}
	if len(builds) != 1 {
		t.Fatalf("expected 1 build, got %d", len(builds))
	}
	pbDir, err := builds[0].ProductBundle.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("GetProductBundleDir failed: %v", err)
	}
	wantDir := filepath.Join(tmpDir, "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle")
	if pbDir != wantDir {
		t.Errorf("got %q, want %q", pbDir, wantDir)
	}
}
