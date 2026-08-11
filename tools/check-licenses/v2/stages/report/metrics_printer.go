// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"fmt"
	"path/filepath"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// SummaryMetricsPrinter prints the execution summary table.
type SummaryMetricsPrinter struct {
	FuchsiaDir string
	StartTime  time.Time
}

// NewSummaryMetricsPrinter creates a new SummaryMetricsPrinter.
func NewSummaryMetricsPrinter(fuchsiaDir string, startTime time.Time) *SummaryMetricsPrinter {
	return &SummaryMetricsPrinter{
		FuchsiaDir: fuchsiaDir,
		StartTime:  startTime,
	}
}

func (p *SummaryMetricsPrinter) Run(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
	duration := time.Since(p.StartTime)
	for _, proj := range projects {
		relProj, _ := filepath.Rel(p.FuchsiaDir, proj.RootPath)
		fmt.Printf("\nUpdate Summary for %s\n", relProj)
		fmt.Printf("----------------------------------------------------\n")
		fmt.Printf("Files discovered in project: %d\n", len(proj.Files))
		fmt.Printf("Files classified:            %d\n", len(proj.ClassifiedFiles))
		fmt.Printf("Files containing licenses:   %d\n", len(proj.FoundLicenses()))
		fmt.Printf("Total execution time:        %v\n", duration)
	}
	return nil
}
