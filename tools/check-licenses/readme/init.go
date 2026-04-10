// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"context"
	"fmt"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/util"
)

var git util.GitInterface

func Initialize() error {
	var err error
	git, err = util.NewGit()
	if err != nil {
		return fmt.Errorf("Failed to create git hook: %w", err)
	}

	// Wrap the git interface to add metrics tracking
	git = &gitMetricsWrapper{git}

	return nil
}

type gitMetricsWrapper struct {
	util.GitInterface
}

func (w *gitMetricsWrapper) GetURL(ctx context.Context, path string) (string, error) {
	defer metrics.GitCommandDuration.Track()()
	return w.GitInterface.GetURL(ctx, path)
}

func (w *gitMetricsWrapper) GetCommitHash(ctx context.Context, path string) (string, error) {
	defer metrics.GitCommandDuration.Track()()
	return w.GitInterface.GetCommitHash(ctx, path)
}

func InitializeForTest() {
	allReadmesMu.Lock()
	allReadmes = make(map[string]*Readme)
	allReadmesMu.Unlock()
	git = gitForTest{}
}

type gitForTest struct {
}

func (g gitForTest) GetURL(ctx context.Context, path string) (string, error) {
	return "www.example.com", nil
}

func (g gitForTest) GetCommitHash(ctx context.Context, path string) (string, error) {
	return "hash", nil
}
