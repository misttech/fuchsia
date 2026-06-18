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

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestGrouper_Run(t *testing.T) {
	fuchsiaDir := t.TempDir()

	grouper := NewGrouper(
		fuchsiaDir,
		[]string{"third_party", filepath.Join("prebuilt", "foo")},
		map[string]string{
			filepath.Join("prebuilt", "virtual"): "/fake/path/to/README.fuchsia",
		},
		false,
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
