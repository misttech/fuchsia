// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"bytes"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
)

// LINT.IfChange(bazel2gn_output)

// SentinelComment separates hand-written GN targets (above) from targets that
// bazel2gn generates from BUILD.bazel (below) in a BUILD.gn file. A BUILD.gn
// containing this line is managed by bazel2gn.
const SentinelComment = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##"

// LINT.ThenChange(//scripts/shac/bazel_migration.star:bazel2gn_output)

// DiscoverSentinelDirectories walks `fuchsiaDir` to find all directories
// whose BUILD.gn files contain SentinelComment, including those in nested
// repositories such as vendor/google. Returned paths are relative to
// `fuchsiaDir`.
//
// Symlinks are not followed, and directories matched by SkipDiscovery are
// skipped.
func DiscoverSentinelDirectories(fuchsiaDir string) ([]string, error) {
	var dirs []string
	err := filepath.WalkDir(fuchsiaDir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			rel, err := filepath.Rel(fuchsiaDir, path)
			if err != nil {
				return err
			}
			if SkipDiscovery(rel) {
				return filepath.SkipDir
			}
			return nil
		}
		if d.Name() != "BUILD.gn" || !d.Type().IsRegular() {
			return nil
		}
		content, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		if bytes.Contains(content, []byte(SentinelComment)) {
			rel, err := filepath.Rel(fuchsiaDir, filepath.Dir(path))
			if err != nil {
				return err
			}
			dirs = append(dirs, rel)
		}
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("discovering bazel2gn sentinel directories in %s: %v", fuchsiaDir, err)
	}
	if len(dirs) == 0 {
		return nil, fmt.Errorf("no BUILD.gn files containing the bazel2gn sentinel found in %s", fuchsiaDir)
	}
	return dirs, nil
}

// SkipDiscovery returns whether to skip the directory `rel`, relative to the
// Fuchsia root directory, when discovering bazel2gn-managed directories. These
// are large directories known to not contain any, and .jiri_root, which holds
// other checkouts created by `fx worktree` (.jiri_root/worktrees/<name>).
func SkipDiscovery(rel string) bool {
	switch rel {
	case ".jiri_root", "out", "prebuilt", "third_party/golibs/vendor", "third_party/rust_crates/vendor":
		return true
	}
	return filepath.Base(rel) == ".git"
}
