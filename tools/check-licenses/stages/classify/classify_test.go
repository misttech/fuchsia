// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package classify

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
)

func TestClassifier_Run(t *testing.T) {
	tempDir := t.TempDir()

	// Create a fake custom pattern file for FuchsiaCopyright to load into the engine
	patternDir := filepath.Join(tempDir, "_Header", "FuchsiaCopyright")
	if err := os.MkdirAll(patternDir, 0755); err != nil {
		t.Fatal(err)
	}
	patternFile := filepath.Join(patternDir, "fuchsia.txt")
	patternContent := []byte(`Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.`)
	if err := os.WriteFile(patternFile, patternContent, 0644); err != nil {
		t.Fatal(err)
	}

	testFilePath := filepath.Join(tempDir, "test.cc")
	content := []byte(`// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

int main() { return 0; }`)
	if err := os.WriteFile(testFilePath, content, 0644); err != nil {
		t.Fatal(err)
	}

	testImgPath := filepath.Join(tempDir, "image.jpg")
	if err := os.WriteFile(testImgPath, []byte("fake image data"), 0644); err != nil {
		t.Fatal(err)
	}

	testBinaryPath := filepath.Join(tempDir, "identify_license")
	if err := os.WriteFile(testBinaryPath, []byte("fake binary data matching a LICENSE pattern somehow"), 0644); err != nil {
		t.Fatal(err)
	}

	classifier, err := NewClassifier(Config{
		Threshold:        0.8,
		PatternDirs:      []string{tempDir},
		TargetExtensions: map[string]bool{".cc": true},
	})
	if err != nil {
		t.Fatalf("Failed to create classifier: %v", err)
	}

	inChan := make(chan pipeline.FilteredProject, 1)

	// Simulate the Pruner stage emitting a project with both files
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: tempDir,
			Files: []pipeline.FileInfo{
				{Path: testFilePath},
				{Path: testImgPath},
				{Path: testBinaryPath, IsNonLicense: true},
			},
		},
	}
	close(inChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	outChan, err := classifier.Run(ctx, inChan)
	if err != nil {
		t.Fatalf("Failed to run classifier: %v", err)
	}

	var results []pipeline.ClassifiedFile
	for cf := range outChan {
		results = append(results, cf)
	}

	if len(results) != 3 {
		t.Fatalf("Expected exactly 3 classified files, got %d", len(results))
	}

	var res pipeline.ClassifiedFile
	var skippedExt pipeline.ClassifiedFile
	var skippedNonLicense pipeline.ClassifiedFile
	for _, r := range results {
		if r.Path == testFilePath {
			res = r
		} else if r.Path == testImgPath {
			skippedExt = r
		} else if r.Path == testBinaryPath {
			skippedNonLicense = r
		}
	}

	if skippedExt.Path != testImgPath {
		t.Errorf("Expected skipped extension path %q, got %q", testImgPath, skippedExt.Path)
	}
	if len(skippedExt.AnalyzedText) > 0 {
		t.Error("Expected skipped extension file to have no AnalyzedText")
	}

	if skippedNonLicense.Path != testBinaryPath {
		t.Errorf("Expected skipped non-license path %q, got %q", testBinaryPath, skippedNonLicense.Path)
	}
	if len(skippedNonLicense.AnalyzedText) > 0 {
		t.Error("Expected skipped non-license file to have no AnalyzedText")
	}
	if skippedNonLicense.IsLicenseFile {
		t.Error("Expected skipped non-license file to not be marked as IsLicenseFile")
	}

	if res.Path != testFilePath {
		t.Errorf("Expected path %q, got %q", testFilePath, res.Path)
	}

	hasCopyright := false
	for _, match := range res.Matches {
		if match.SPDXID == "FuchsiaCopyright" || match.MatchType == "Copyright" {
			hasCopyright = true
			if len(match.Text) == 0 {
				t.Error("Expected matched text to be populated, got empty bytes")
			}
		}
	}

	if !hasCopyright {
		t.Errorf("Expected FuchsiaCopyright match to be found in file. Found matches: %v", res.Matches)
	}
}

func TestExpandNoticeStartLine(t *testing.T) {
	tests := []struct {
		name           string
		content        string
		matchStartLine int
		minLine        int
		wantStartLine  int
	}{
		{
			name: "C block comment with preceding copyright",
			content: `/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Redistribution and use in source and binary forms...
 */`,
			matchStartLine: 4,
			minLine:        1,
			wantStartLine:  1,
		},
		{
			name: "C block comment preceded by separate descriptive block comment",
			content: `/* dscp lookup routines lifted wholesale from openssh */

/*
 * Copyright (c) 2000 Markus Friedl. All rights reserved.
 *
 * Redistribution and use in source and binary forms...
 */`,
			matchStartLine: 6,
			minLine:        1,
			wantStartLine:  3,
		},
		{
			name: "Second C block comment after first license match",
			content: `/*
 * Copyright 2006 Oracle
 *
 * Permission is hereby granted...
 */
/*
 * Copyright 2002 Keith Packard
 *
 * Permission to use, copy, modify...
 */`,
			matchStartLine: 9,
			minLine:        5,
			wantStartLine:  6,
		},
		{
			name: "Single-line // comment block",
			content: `// Copyright (c) 2022 The Fuchsia Authors
//
// Permission to use, copy, modify...`,
			matchStartLine: 3,
			minLine:        1,
			wantStartLine:  1,
		},
		{
			name: "Single-line # comment block with shebang",
			content: `#!/usr/bin/env python3
# Copyright 2017 The Fuchsia Authors. All rights reserved.
#
# Redistribution and use in source and binary forms...`,
			matchStartLine: 4,
			minLine:        1,
			wantStartLine:  2,
		},
		{
			name: "C block comment after #ifndef header guard",
			content: `#ifndef FOO_H
#define FOO_H
/*
 * Copyright 2020 Author
 *
 * Permission to use...
 */`,
			matchStartLine: 6,
			minLine:        1,
			wantStartLine:  3,
		},
		{
			name: "C block comment preceded by adjacent single-line copyright block comment",
			content: `/* Copyright (C) 2005, 2007, 2008, 2013 by George Williams */
/*
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 */`,
			matchStartLine: 3,
			minLine:        1,
			wantStartLine:  1,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			lines := splitLinesForTest([]byte(tc.content))
			got := expandNoticeStartLine(lines, tc.matchStartLine, tc.minLine)
			if got != tc.wantStartLine {
				t.Errorf("expandNoticeStartLine() = %d, want %d", got, tc.wantStartLine)
			}
		})
	}
}

func splitLinesForTest(b []byte) [][]byte {
	var lines [][]byte
	start := 0
	for i := 0; i < len(b); i++ {
		if b[i] == '\n' {
			lines = append(lines, b[start:i])
			start = i + 1
		}
	}
	lines = append(lines, b[start:])
	return lines
}
