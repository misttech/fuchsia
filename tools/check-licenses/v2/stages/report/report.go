// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Reporter is a composite Renderer that composes modular renderers
// (ReadmeVerifier, ReadmeWriter, NoticeRenderer, SpdxRenderer, ConsoleErrorReporter)
// based on configuration flags.
type Reporter struct {
	FuchsiaDir string
	OutDir     string
	Config     Config
}

// NewReporter creates a new Reporter.
func NewReporter(fuchsiaDir, outDir string, config Config) *Reporter {
	return &Reporter{
		FuchsiaDir: fuchsiaDir,
		OutDir:     outDir,
		Config:     config,
	}
}

func (r *Reporter) Run(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
	var renderers pipeline.MultiRenderer

	if r.Config.GenerateArtifacts {
		renderers = append(renderers, NewNoticeRenderer(r.OutDir), NewSpdxRenderer(r.OutDir))
	}

	return renderers.Run(ctx, projects, errors)
}
