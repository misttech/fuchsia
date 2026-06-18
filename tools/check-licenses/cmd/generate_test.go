// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

func TestCustomClassifier_Run(t *testing.T) {
	tempDir := t.TempDir()

	// Scaffold patterns for base classifier
	patternsDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns")
	copyrightPatternDir := filepath.Join(patternsDir, "_Header", "FuchsiaCopyright")
	os.MkdirAll(copyrightPatternDir, 0755)
	os.WriteFile(filepath.Join(copyrightPatternDir, "fuchsia.txt"), []byte("// Copyright 2026 The Fuchsia Authors. All rights reserved.\n"), 0644)

	baseClassifier, err := classify.NewClassifier(0.8, []string{patternsDir}, map[string]bool{".cc": true})
	if err != nil {
		t.Fatal(err)
	}

	cc := &CustomClassifier{
		Base:       baseClassifier,
		FuchsiaDir: tempDir,
	}

	// Scaffold project and files
	projDir := filepath.Join(tempDir, "my_project")
	os.MkdirAll(projDir, 0755)

	// 1. Dedicated License File
	licenseFile := filepath.Join(projDir, "LICENSE")
	os.WriteFile(licenseFile, []byte("This is a custom verbatim license."), 0644)

	// 2. Source File with header
	sourceFile := filepath.Join(projDir, "main.cc")
	os.WriteFile(sourceFile, []byte("// Copyright 2026 The Fuchsia Authors. All rights reserved.\nvoid main() {}"), 0644)

	// 3. README.fuchsia designating them
	readmeContent := `Name: my_project
URL: http://foo
Version: 1.0
Revision: abc
Security Critical: no
License File: LICENSE
Source File: main.cc
`
	os.WriteFile(filepath.Join(projDir, "README.fuchsia"), []byte(readmeContent), 0644)

	inChan := make(chan pipeline.FilteredProject, 1)
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: projDir,
			Files: []pipeline.FileInfo{
				{Path: licenseFile},
				{Path: sourceFile},
			},
		},
	}
	close(inChan)

	ctx := context.Background()
	outChan, err := cc.Run(ctx, inChan)
	if err != nil {
		t.Fatal(err)
	}

	var classifiedFiles []pipeline.ClassifiedFile
	for cf := range outChan {
		classifiedFiles = append(classifiedFiles, cf)
	}

	cc.PrintErrors()
	for _, cf := range classifiedFiles {
		t.Logf("Classified file: %s", cf.Path)
	}

	if len(classifiedFiles) != 1 {
		t.Fatalf("Expected 1 classified file (verbatim LICENSE), got %d", len(classifiedFiles))
	}

	cf := classifiedFiles[0]
	if cf.Path != licenseFile {
		t.Errorf("Expected classified file to be %s, got %s", licenseFile, cf.Path)
	}
	if !cf.IsLicenseFile {
		t.Errorf("Expected LICENSE to be marked as license file")
	}
	if len(cf.Matches) != 1 {
		t.Errorf("Expected 1 match for LICENSE, got %d", len(cf.Matches))
	} else if string(cf.Matches[0].Text) != "This is a custom verbatim license." {
		t.Errorf("Expected verbatim text, got %s", string(cf.Matches[0].Text))
	}

	// Verify that main.cc triggered an error because classifier couldn't find a match
	if len(cc.Errors) != 1 {
		t.Errorf("Expected 1 error for main.cc, got %d", len(cc.Errors))
	} else if !strings.Contains(cc.Errors[0], "Classifier could not detect a license in Source File") {
		t.Errorf("Expected error message to mention classifier failure, got: %s", cc.Errors[0])
	}
}
