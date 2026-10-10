// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func writeFile(t *testing.T, path string, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestDiscoverSentinelDirectories(t *testing.T) {
	root := t.TempDir()
	withSentinel := "# Preserved.\n\n" + sentinelComment + "\n\ngroup(\"foo\") {}\n"
	writeFile(t, filepath.Join(root, "BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, "a/b/BUILD.gn"), withSentinel)
	// Nested repositories are included.
	writeFile(t, filepath.Join(root, "vendor/google/c/BUILD.gn"), withSentinel)
	// No sentinel.
	writeFile(t, filepath.Join(root, "d/BUILD.gn"), "group(\"foo\") {}\n")
	// Only BUILD.gn files are considered.
	writeFile(t, filepath.Join(root, "e/BUILD.bazel"), "# "+sentinelComment+"\n")
	// Skipped directories.
	writeFile(t, filepath.Join(root, "out/default/BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, "prebuilt/f/BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, "third_party/golibs/vendor/BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, "third_party/rust_crates/vendor/g/BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, ".git/BUILD.gn"), withSentinel)
	writeFile(t, filepath.Join(root, "vendor/google/.git/BUILD.gn"), withSentinel)

	got, err := discoverSentinelDirectories(root)
	if err != nil {
		t.Fatalf("discoverSentinelDirectories() error: %v", err)
	}
	want := []string{".", "a/b", "vendor/google/c"}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("discoverSentinelDirectories() diff (-want +got):\n%s", diff)
	}
}
