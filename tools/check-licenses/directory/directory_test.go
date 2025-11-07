// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package directory

import (
	"embed"
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
)

//go:embed testdata/*
var testDataFS embed.FS

// dumpTestDataFS recursively writes the content of the embedded file system to
// the specified directory.
func dumpTestDataFS(path string) error {
	return fs.WalkDir(testDataFS, ".", func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return os.MkdirAll(filepath.Join(path, p), 0755)
		}
		data, err := testDataFS.ReadFile(p)
		if err != nil {
			return err
		}
		return os.WriteFile(filepath.Join(path, p), data, 0644)
	})
}

// NewDirectory(empty) should produce a directory object that correctly
// represents an empty directory.
func TestDirectoryCreateEmpty(t *testing.T) {
	runDirectoryTest("empty", t)
}

// NewDirectory(simple) should produce a directory object that correctly
// represents the simple testdata directory.
func TestDirectoryCreateSimple(t *testing.T) {
	runDirectoryTest("simple", t)
}

// NewDirectory(skip) should produce a directory object that correctly
// skips the configured directories.
func TestDirectoryWithSkips(t *testing.T) {
	runDirectoryTest("skipdir", t)
}

func runDirectoryTest(name string, t *testing.T) {
	t.Helper()

	tempDir := t.TempDir()
	// Note here we are copying more than we need, e.g. we copy the entire testdata
	// directory, but we only need the files in the testdata/{name} directory.
	// This is done for simplicity, which is OK since the testdata directory is small.
	if err := dumpTestDataFS(tempDir); err != nil {
		t.Fatal(err)
	}
	testDataDir := filepath.Join(tempDir, "testdata")

	// Create a Directory object from the want.json file.
	want := &Directory{}
	decodeJSON(filepath.Join(testDataDir, name, "want.json"), want, t)

	config := NewConfig()
	decodeJSON(filepath.Join(testDataDir, name, "config.json"), config, t)
	root := filepath.Join(testDataDir, name, "root")
	cleanConfig(config, root)

	// Set the FuchsiaDir for the file package, to get predictable hashes,
	// which are based on relative paths to the FuchsiaDir.
	origFuchsiaDir := file.Config.FuchsiaDir
	t.Cleanup(func() { file.Config.FuchsiaDir = origFuchsiaDir })
	file.Config.FuchsiaDir = tempDir

	got, err := newDirectoryWithConfig(root, nil, config)
	if err != nil {
		t.Fatal(err)
	}

	diffDirectories(want, got, t)
}

func cleanConfig(c *DirectoryConfig, root string) {
	for _, s := range c.Skips {
		for i, p := range s.Paths {
			s.Paths[i] = strings.ReplaceAll(p, "{root}", root)
		}
	}
}

func decodeJSON(path string, obj interface{}, t *testing.T) {
	t.Helper()

	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	decoder := json.NewDecoder(strings.NewReader(string(contents)))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(obj); err != nil {
		t.Fatalf("%v: failed to decode %s into struct: %v.", t.Name(), path, err)
	}
}

func diffDirectories(want, got *Directory, t *testing.T) {
	t.Helper()

	if want.Name != got.Name {
		t.Errorf("%s: directory name mismatch: (-want +got):\n-%s\n+%s", t.Name(), want.Name, got.Name)
	}

	if len(want.Files) != len(got.Files) {
		t.Errorf("%s: files length mismatch:(-want +got):\n-%d\n+%d", t.Name(), len(want.Files), len(got.Files))
	}
	for i := range want.Files {
		w := want.Files[i]
		g := got.Files[i]

		if w.Name() != g.Name() {
			t.Errorf("%s: file names mismatch:(-want +got):\n-%s\n+%s", t.Name(), w.Name(), g.Name())
		}
		if w.SPDXID() != g.SPDXID() {
			t.Errorf("%s: file SPDXID mismatch:(-want +got):\n-%s\n+%s", t.Name(), w.SPDXID(), g.SPDXID())
		}
	}

	if len(want.Children) != len(got.Children) {
		t.Errorf("%s: children length mismatch:(-want +got):\n-%d\n+%d", t.Name(), len(want.Children), len(got.Children))
	}
	for i := range want.Children {
		diffDirectories(want.Children[i], got.Children[i], t)
	}
}
