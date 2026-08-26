// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
)

// Finding represents a single structured static analysis finding for shac / Gerrit.
type Finding struct {
	FilePath  string `json:"filepath,omitempty"`
	Line      int    `json:"line,omitempty"`
	EndLine   int    `json:"end_line,omitempty"`
	Level     string `json:"level"`
	Message   string `json:"message"`
	CheckName string `json:"check_name,omitempty"`
}

// FindingsReporter writes validation errors to a structured JSON file.
type FindingsReporter struct {
	FuchsiaDir   string
	FindingsFile string
}

// NewFindingsReporter creates a new FindingsReporter.
func NewFindingsReporter(fuchsiaDir, findingsFile string) *FindingsReporter {
	return &FindingsReporter{
		FuchsiaDir:   fuchsiaDir,
		FindingsFile: findingsFile,
	}
}

// Run collects ComplianceErrors, converts them to Findings, and writes them to FindingsFile.
func (r *FindingsReporter) Run(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
	if r.FindingsFile == "" {
		return nil
	}

	findings := []Finding{}
	for _, e := range errors {
		relPath := ""
		if e.FilePath != "" {
			var err error
			relPath, err = filepath.Rel(r.FuchsiaDir, e.FilePath)
			if err != nil || relPath == "." || relPath == "" {
				relPath = e.FilePath
			}
			relPath = filepath.ToSlash(relPath)
		}

		startLine := e.StartLine
		endLine := e.EndLine
		if startLine > 0 && endLine < startLine {
			endLine = startLine
		}

		findings = append(findings, Finding{
			FilePath:  relPath,
			Line:      startLine,
			EndLine:   endLine,
			Level:     "error",
			Message:   e.Issue,
			CheckName: e.CheckName,
		})
	}

	sort.Slice(findings, func(i, j int) bool {
		if findings[i].FilePath != findings[j].FilePath {
			return findings[i].FilePath < findings[j].FilePath
		}
		if findings[i].Line != findings[j].Line {
			return findings[i].Line < findings[j].Line
		}
		return findings[i].CheckName < findings[j].CheckName
	})

	data, err := json.MarshalIndent(findings, "", "  ")
	if err != nil {
		return err
	}

	if dir := filepath.Dir(r.FindingsFile); dir != "" && dir != "." {
		if err := os.MkdirAll(dir, 0755); err != nil {
			return err
		}
	}

	return os.WriteFile(r.FindingsFile, append(data, '\n'), 0644)
}
