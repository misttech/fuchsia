// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func writeTestFile(t *testing.T, path string, content string) {
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
	withSentinel := "# Preserved.\n\n" + SentinelComment + "\n\ngroup(\"foo\") {}\n"
	writeTestFile(t, filepath.Join(root, "BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, "a/b/BUILD.gn"), withSentinel)
	// Nested repositories are included.
	writeTestFile(t, filepath.Join(root, "vendor/google/c/BUILD.gn"), withSentinel)
	// No sentinel.
	writeTestFile(t, filepath.Join(root, "d/BUILD.gn"), "group(\"foo\") {}\n")
	// Only BUILD.gn files are considered.
	writeTestFile(t, filepath.Join(root, "e/BUILD.bazel"), "# "+SentinelComment+"\n")
	// Skipped directories.
	writeTestFile(t, filepath.Join(root, "out/default/BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, "prebuilt/f/BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, "third_party/golibs/vendor/BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, "third_party/rust_crates/vendor/g/BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, ".git/BUILD.gn"), withSentinel)
	// Other checkouts created by `fx worktree`.
	writeTestFile(t, filepath.Join(root, ".jiri_root/worktrees/w/a/BUILD.gn"), withSentinel)
	writeTestFile(t, filepath.Join(root, "vendor/google/.git/BUILD.gn"), withSentinel)

	got, err := DiscoverSentinelDirectories(root)
	if err != nil {
		t.Fatalf("DiscoverSentinelDirectories() error: %v", err)
	}
	want := []string{".", "a/b", "vendor/google/c"}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("DiscoverSentinelDirectories() diff (-want +got):\n%s", diff)
	}
}
