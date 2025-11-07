// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"embed"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/testutil"
)

//go:embed testdata/*
var testDataFS embed.FS

func TestNameLicenseProvided(t *testing.T) {
	testDataDir := setup(t)
	name := "Test Readme Project"

	path := filepath.Join(testDataDir, "happy", "README.fuchsia")
	r, err := readme.NewReadmeFromFile(path)
	if err != nil {
		t.Fatalf("%v: expected no error, got %v.", t.Name(), err)
	}

	p, err := NewProject(r, filepath.Dir(path))
	if err != nil {
		t.Fatalf("%v: expected no error, got %v.", t.Name(), err)
	}
	if p.Name != name {
		t.Errorf("%v: expected Name == \"%v\", got %v.", t.Name(), name, p.Name)
	}
}

func setup(t *testing.T) string {
	tempDir := t.TempDir()
	testutil.DumpTestData(t, testDataFS, tempDir)
	file.Config = file.NewConfig()
	Config = NewConfig()
	Config.FuchsiaDir = tempDir
	Initialize(Config)
	readme.InitializeForTest()
	return filepath.Join(tempDir, "testdata")
}
