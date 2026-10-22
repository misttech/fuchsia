// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"encoding/json"
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

// fakeTool writes an executable shell script to a temporary directory.
func fakeTool(t *testing.T, name, script string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), name)
	if err := os.WriteFile(path, []byte("#!/bin/sh\n"+script), 0o755); err != nil {
		t.Fatalf("writing fake %s: %s", name, err)
	}
	return path
}

func TestJSONDiagnostics(t *testing.T) {
	fuchsiaDir := t.TempDir()
	sentinelGN := sentinelComment + "\n"

	writeFile(t, filepath.Join(fuchsiaDir, "out_of_sync", "BUILD.bazel"), "")
	writeFile(t, filepath.Join(fuchsiaDir, "out_of_sync", "BUILD.gn"), sentinelGN+"# edited\n")
	writeFile(t, filepath.Join(fuchsiaDir, "unconvertible", "BUILD.bazel"), "load(\"//foo.bzl\", \"foo\")\n\nunknown_rule(name = \"x\")\n")
	writeFile(t, filepath.Join(fuchsiaDir, "unconvertible", "BUILD.gn"), sentinelGN)
	writeFile(t, filepath.Join(fuchsiaDir, "unformatted", "BUILD.bazel"), "unknown_rule(name = \"x\")  # UNFORMATTED\n")
	writeFile(t, filepath.Join(fuchsiaDir, "unformatted", "BUILD.gn"), sentinelGN)
	writeFile(t, filepath.Join(fuchsiaDir, "syntax_error", "BUILD.bazel"), "foo(\n  a = ,\n)\n")
	writeFile(t, filepath.Join(fuchsiaDir, "syntax_error", "BUILD.gn"), sentinelGN)
	writeFile(t, filepath.Join(fuchsiaDir, "no_bazel", "BUILD.gn"), sentinelGN)
	writeFile(t, filepath.Join(fuchsiaDir, "missing_crate", "BUILD.bazel"), "rustc_library(name = \"lib\")\n\nrustc_test(\n    name = \"test\",\n    crate = \":missing\",\n)\n")
	writeFile(t, filepath.Join(fuchsiaDir, "missing_crate", "BUILD.gn"), sentinelGN)

	p := syncParams{
		// Simulate buildifier: fail on the syntax error, strip the
		// UNFORMATTED marker, and otherwise pass the file through.
		buildifierBin: fakeTool(t, "buildifier", `
input="$(cat)"
case "$input" in
  *"a = ,"*) printf 'warning: something\n<stdin>:2:8: syntax error near ,\n' >&2; exit 1 ;;
esac
printf '%s\n' "$input" | sed 's/  # UNFORMATTED//'
`),
		gnBin:     fakeTool(t, "gn", "cat\n"),
		checkOnly: true,
	}
	dirs := []string{"missing_crate", "unconvertible", "out_of_sync/", "./no_bazel", "syntax_error", "unformatted", "/elsewhere"}
	errs := runBatch(context.Background(), dirs, fuchsiaDir, 2, p)

	var buf bytes.Buffer
	if err := writeDiagnostics(&buf, fuchsiaDir, errs); err != nil {
		t.Fatalf("writeDiagnostics() failed: %s", err)
	}
	var got []diagnostic
	if err := json.Unmarshal(buf.Bytes(), &got); err != nil {
		t.Fatalf("parsing writeDiagnostics() output %q: %s", buf.String(), err)
	}
	// Diffs are long and not the subject of this test.
	for i, d := range got {
		if d.OutOfSync && d.Message != "" {
			got[i].Message = "<diff>"
		}
	}
	want := []diagnostic{
		{Directory: "/elsewhere", Message: "directory is not inside --fuchsia_dir " + fuchsiaDir},
		{Directory: "missing_crate", File: "missing_crate/BUILD.bazel", Line: 3, Message: `crate ":missing" of rustc_test is not defined in the same BUILD.bazel`},
		{Directory: "no_bazel", File: "no_bazel/BUILD.gn", Message: "BUILD.gn has the bazel2gn sentinel but there's no BUILD.bazel next to it"},
		{Directory: "out_of_sync", File: "out_of_sync/BUILD.gn", OutOfSync: true, Message: "<diff>"},
		{Directory: "syntax_error", File: "syntax_error/BUILD.bazel", Line: 2, Message: "syntax error near ,"},
		{Directory: "unconvertible", File: "unconvertible/BUILD.bazel", Line: 3, Message: "unknown_rule is not a known Bazel rule to convert to GN"},
		// Lines in the formatted content don't necessarily match the
		// unformatted file on disk.
		{Directory: "unformatted", File: "unformatted/BUILD.bazel", Message: "unknown_rule is not a known Bazel rule to convert to GN"},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("writeDiagnostics(runBatch(%q)) diff (-want +got):\n%s", dirs, diff)
	}
}
