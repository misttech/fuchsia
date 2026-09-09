// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package boundary

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
)

func TestGrouper_Run(t *testing.T) {
	fuchsiaDir := t.TempDir()

	grouper := NewGrouper(
		fuchsiaDir,
		Config{
			BarrierPaths: map[string]bool{"third_party": true, filepath.Join("prebuilt", "foo"): true},
			OutOfTreeReadmes: map[string]string{
				filepath.Join("prebuilt", "virtual"): "/fake/path/to/README.fuchsia",
			},
			FilesInReadmeOnly: false,
		},
	)

	inChan := make(chan pipeline.RawPath, 10)

	// 1. File with a physical README in same dir
	proj1DirRel := filepath.Join("src", "proj1")
	proj1Dir := filepath.Join(fuchsiaDir, proj1DirRel)
	if err := os.MkdirAll(proj1Dir, 0755); err != nil {
		t.Fatal(err)
	}
	readmeContent := []byte(fmt.Sprintf(`License: Android
License File: %s

-------------------- DEPENDENCY DIVIDER --------------------

Location: vendored_lib
License: Chromium
License File: %s
`, filepath.Join(proj1DirRel, "lib", "util.cc"), filepath.Join(proj1DirRel, "vendored_lib", "LICENSE")))

	if err := os.WriteFile(filepath.Join(proj1Dir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	inChan <- pipeline.RawPath{Path: filepath.Join(proj1Dir, "README.fuchsia"), IsDir: false}
	inChan <- pipeline.RawPath{Path: filepath.Join(proj1Dir, "main.cc"), IsDir: false}

	// 2. File in a child dir of a physical README
	inChan <- pipeline.RawPath{Path: filepath.Join(proj1Dir, "lib", "util.cc"), IsDir: false}

	// 2.5 File in a sub-project (vendored_lib) defined by DEPENDENCY DIVIDER Location
	subProjDir := filepath.Join(proj1Dir, "vendored_lib")
	inChan <- pipeline.RawPath{Path: filepath.Join(subProjDir, "sub_main.cc"), IsDir: false}

	// 3. File behind a Barrier (third_party/foo should be project root)
	proj2Dir := filepath.Join(fuchsiaDir, "third_party", "foo")
	inChan <- pipeline.RawPath{Path: filepath.Join(proj2Dir, "src", "bar.cc"), IsDir: false}

	// 5. File behind a Virtual README
	proj4Dir := filepath.Join(fuchsiaDir, "prebuilt", "virtual")
	inChan <- pipeline.RawPath{Path: filepath.Join(proj4Dir, "bin", "tool"), IsDir: false}

	close(inChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	outChan, err := grouper.Run(ctx, inChan)
	if err != nil {
		t.Fatalf("Failed to run grouper: %v", err)
	}

	results := make(map[string][]pipeline.FileInfo)
	for p := range outChan {
		results[p.RootPath] = p.Files
	}

	expectedProj1 := []pipeline.FileInfo{
		{Path: filepath.Join(proj1Dir, "README.fuchsia")},
		{Path: filepath.Join(proj1Dir, "lib", "util.cc"), IsLicenseFile: true},
		{Path: filepath.Join(proj1Dir, "main.cc")},
	}
	if !reflect.DeepEqual(results[proj1Dir], expectedProj1) {
		t.Errorf("Expected proj1 files %v, got %v", expectedProj1, results[proj1Dir])
	}

	expectedSubProj := []pipeline.FileInfo{
		{Path: filepath.Join(subProjDir, "sub_main.cc")},
	}
	if !reflect.DeepEqual(results[subProjDir], expectedSubProj) {
		t.Errorf("Expected subProj files %v, got %v", expectedSubProj, results[subProjDir])
	}

	expectedProj2 := []pipeline.FileInfo{{Path: filepath.Join(proj2Dir, "src", "bar.cc")}}
	if !reflect.DeepEqual(results[proj2Dir], expectedProj2) {
		t.Errorf("Expected proj2 files %v, got %v", expectedProj2, results[proj2Dir])
	}

	expectedProj4 := []pipeline.FileInfo{{Path: filepath.Join(proj4Dir, "bin", "tool")}}
	if !reflect.DeepEqual(results[proj4Dir], expectedProj4) {
		t.Errorf("Expected proj4 files %v, got %v", expectedProj4, results[proj4Dir])
	}
}

func TestGrouper_VirtualReadmeWithPackageManifest(t *testing.T) {
	fuchsiaDir := t.TempDir()

	virtualReadmePath := filepath.Join(fuchsiaDir, "virtual_readmes", "golibs_README.fuchsia")
	if err := os.MkdirAll(filepath.Dir(virtualReadmePath), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(virtualReadmePath, []byte("Name: golibs\n"), 0644); err != nil {
		t.Fatal(err)
	}

	golibsDir := filepath.Join(fuchsiaDir, "third_party", "golibs")
	if err := os.MkdirAll(golibsDir, 0755); err != nil {
		t.Fatal(err)
	}

	goModContent := `module go.fuchsia.dev/fuchsia/third_party/golibs

go 1.23

require (
	github.com/spdx/tools-golang v0.5.5
)
`
	goModPath := filepath.Join(golibsDir, "go.mod")
	if err := os.WriteFile(goModPath, []byte(goModContent), 0644); err != nil {
		t.Fatal(err)
	}

	spdxDir := filepath.Join(golibsDir, "vendor", "github.com/spdx/tools-golang")
	if err := os.MkdirAll(spdxDir, 0755); err != nil {
		t.Fatal(err)
	}

	grouper := NewGrouper(
		fuchsiaDir,
		Config{
			BarrierPaths: map[string]bool{"third_party": true},
			OutOfTreeReadmes: map[string]string{
				filepath.Join("third_party", "golibs"): virtualReadmePath,
			},
		},
	)

	inChan := make(chan pipeline.RawPath, 10)
	inChan <- pipeline.RawPath{Path: goModPath, IsDir: false}
	inChan <- pipeline.RawPath{Path: filepath.Join(spdxDir, "LICENSE.code"), IsDir: false}
	inChan <- pipeline.RawPath{Path: filepath.Join(spdxDir, "main.go"), IsDir: false}
	close(inChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	outChan, err := grouper.Run(ctx, inChan)
	if err != nil {
		t.Fatalf("Failed to run grouper: %v", err)
	}

	results := make(map[string][]pipeline.FileInfo)
	for p := range outChan {
		results[p.RootPath] = p.Files
	}

	// Verify spdx files are grouped under the vendored package root, not squashed into third_party/golibs
	spdxFiles, ok := results[spdxDir]
	if !ok {
		t.Fatalf("Expected group for spdxDir %s, got groups: %v", spdxDir, results)
	}
	expectedPaths := []string{
		filepath.Join(spdxDir, "LICENSE.code"),
		filepath.Join(spdxDir, "main.go"),
	}
	var actualPaths []string
	for _, f := range spdxFiles {
		actualPaths = append(actualPaths, f.Path)
	}
	if !reflect.DeepEqual(actualPaths, expectedPaths) {
		t.Errorf("Expected files %v, got %v", expectedPaths, actualPaths)
	}
}
