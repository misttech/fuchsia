// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestNewWorkspaceContext(t *testing.T) {
	tempDir := t.TempDir()

	// Create a fake fuchsia root
	fuchsiaDir := filepath.Join(tempDir, "fuchsia")
	if err := os.Mkdir(fuchsiaDir, 0755); err != nil {
		t.Fatal(err)
	}

	// Case 1: Simple execution with fuchsia-dir
	ctx, err := NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}
	if ctx.FuchsiaDir != fuchsiaDir {
		t.Errorf("expected FuchsiaDir %s, got %s", fuchsiaDir, ctx.FuchsiaDir)
	}

	// Case 2: .fx-build-dir resolution
	buildRel := "out/default"
	if err := os.MkdirAll(filepath.Join(fuchsiaDir, buildRel), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(fuchsiaDir, ".fx-build-dir"), []byte(buildRel+"\n"), 0644); err != nil {
		t.Fatal(err)
	}

	ctx, err = NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}
	expectedBuildDir := filepath.Join(fuchsiaDir, buildRel)
	if ctx.BuildDir != expectedBuildDir {
		t.Errorf("expected BuildDir %s, got %s", expectedBuildDir, ctx.BuildDir)
	}

	// Case 3: Overriding build-dir via flag
	otherBuildDir := filepath.Join(tempDir, "other_build")
	if err := os.Mkdir(otherBuildDir, 0755); err != nil {
		t.Fatal(err)
	}
	ctx, err = NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir, "--build-dir", otherBuildDir})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}
	if ctx.BuildDir != otherBuildDir {
		t.Errorf("expected BuildDir %s, got %s", otherBuildDir, ctx.BuildDir)
	}

	// Case 4: File list resolution
	file1 := filepath.Join(fuchsiaDir, "src/main.rs")
	if err := os.MkdirAll(filepath.Dir(file1), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(file1, []byte(""), 0644); err != nil {
		t.Fatal(err)
	}

	ctx, err = NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir, "--file", "src/main.rs"})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}
	if len(ctx.Files) != 1 {
		t.Fatalf("expected 1 file, got %d", len(ctx.Files))
	}
	if ctx.Files[0].AbsPath != file1 {
		t.Errorf("expected AbsPath %s, got %s", file1, ctx.Files[0].AbsPath)
	}
	if ctx.Files[0].Status != StatusFound {
		t.Errorf("expected status found, got %s", ctx.Files[0].Status)
	}

	// Case 5: Missing file resolution
	ctx, err = NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir, "--file", "src/missing.rs"})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}
	if ctx.Files[0].Status != StatusNotFound {
		t.Errorf("expected status not_found, got %s", ctx.Files[0].Status)
	}
}

func TestDeduplication(t *testing.T) {
	tempDir := t.TempDir()
	fuchsiaDir := filepath.Join(tempDir, "fuchsia")
	if err := os.Mkdir(fuchsiaDir, 0755); err != nil {
		t.Fatal(err)
	}

	// Create a file and a symlink to it
	realFile := filepath.Join(fuchsiaDir, "real.txt")
	os.WriteFile(realFile, []byte("hello"), 0644)
	linkFile := filepath.Join(fuchsiaDir, "link.txt")
	os.Symlink("real.txt", linkFile)

	// Context with both the real file and the link
	ctx, err := NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir, "--file", "real.txt", "--file", "link.txt"})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}

	// Both should resolve to realFile, so we should have only 1 entry.
	if len(ctx.Files) != 1 {
		t.Errorf("expected 1 file after deduplication, got %d", len(ctx.Files))
	}
	if ctx.Files[0].AbsPath != realFile {
		t.Errorf("expected AbsPath %s, got %s", realFile, ctx.Files[0].AbsPath)
	}
	// The OriginalPath should be of the last occurrence
	if ctx.Files[0].OriginalPath != "link.txt" {
		t.Errorf("expected OriginalPath link.txt, got %s", ctx.Files[0].OriginalPath)
	}
}

func TestFileList(t *testing.T) {
	tempDir := t.TempDir()
	fuchsiaDir := filepath.Join(tempDir, "fuchsia")
	if err := os.Mkdir(fuchsiaDir, 0755); err != nil {
		t.Fatal(err)
	}

	// Create a file-list
	listFile := filepath.Join(tempDir, "myfilelist.txt")
	content := []byte("file1.rs\n# comment\n  file2.rs  \n")
	if err := os.WriteFile(listFile, content, 0644); err != nil {
		t.Fatal(err)
	}

	ctx, err := NewWorkspaceContext([]string{"--fuchsia-dir", fuchsiaDir, "--file-list", listFile})
	if err != nil {
		t.Fatalf("NewWorkspaceContext failed: %v", err)
	}

	if len(ctx.Files) != 2 {
		t.Errorf("expected 2 files from list, got %d", len(ctx.Files))
	}

	expectedOriginalPaths := []string{"file1.rs", "file2.rs"}
	var gotOriginalPaths []string
	for _, f := range ctx.Files {
		gotOriginalPaths = append(gotOriginalPaths, f.OriginalPath)
	}
	if !reflect.DeepEqual(gotOriginalPaths, expectedOriginalPaths) {
		t.Errorf("expected original paths %v, got %v", expectedOriginalPaths, gotOriginalPaths)
	}
}

func TestCanonicalizeMissing(t *testing.T) {
	tempDir := t.TempDir()
	// /tempDir/existing/
	existing := filepath.Join(tempDir, "existing")
	os.Mkdir(existing, 0755)

	// Create symlink: /tempDir/link_to_existing -> /tempDir/existing
	link := filepath.Join(tempDir, "link_to_existing")
	os.Symlink("existing", link)

	// Path: /tempDir/link_to_existing/missing.txt
	path := filepath.Join(link, "missing.txt")

	canonical, status, isDir, err := canonicalize(path)
	if err != nil {
		t.Fatalf("canonicalize failed: %v", err)
	}

	expected := filepath.Join(existing, "missing.txt")
	if canonical != expected {
		t.Errorf("expected %s, got %s", expected, canonical)
	}
	if status != StatusNotFound {
		t.Errorf("expected status not_found, got %s", status)
	}
	if isDir {
		t.Error("expected isDir=false for missing file")
	}
}
