// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

func (c *ProjectCommand) checkFile(ctx context.Context, absPath string, config *v2config.MasterConfig, classifier *classify.Classifier) error {
	// Skip if it doesn't match target extensions, UNLESS it's a dedicated license file
	ext := filepath.Ext(absPath)
	if len(config.TargetExtensions) > 0 && !config.TargetExtensions[ext] && !classify.IsLicenseFilename(absPath) {
		// It's a binary or something else we don't care about
		return nil
	}

	cf, err := runClassifierOnFile(ctx, classifier, absPath)
	if err != nil {
		return err
	}

	// Does it have any non-Copyright matches?
	hasLicense := false
	var unexpectedMatches []string
	for _, match := range cf.Matches {
		if match.MatchType != "Copyright" {
			hasLicense = true
			unexpectedMatches = append(unexpectedMatches, match.SPDXID)
		}
	}

	if !hasLicense {
		// No license text found, we don't care if it's in the README or not.
		return nil
	}

	// It has a license! We need to ensure it's declared in the closest README.fuchsia.
	r, readmePath, err := readme.FindProjectReadme(absPath, c.fuchsiaDir, config.OutOfTreeReadmes)
	if err != nil {
		return fmt.Errorf("found license texts (%s) but failed to find/parse a parent README.fuchsia: %w", strings.Join(unexpectedMatches, ", "), err)
	}
	if r == nil {
		return fmt.Errorf("found license texts (%s) but could not find a parent README.fuchsia in the directory tree", strings.Join(unexpectedMatches, ", "))
	}

	// Check if the file is declared
	relToReadme, err := filepath.Rel(filepath.Dir(readmePath), absPath)
	if err != nil {
		return err
	}

	// Also handle the case where it might be relative to FuchsiaDir in legacy configs, but v2 uses relative to Readme.
	relToFuchsia, _ := filepath.Rel(c.fuchsiaDir, absPath)

	isDeclared := false
	for _, lf := range r.LicenseFiles {
		if filepath.Clean(lf.Path) == relToReadme || filepath.Clean(lf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}
	for _, sf := range r.SourceFiles {
		if filepath.Clean(sf.Path) == relToReadme || filepath.Clean(sf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}
	for _, nlf := range r.NonLicenseFiles {
		if filepath.Clean(nlf.Path) == relToReadme || filepath.Clean(nlf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}

	if !isDeclared {
		return fmt.Errorf("file contains license texts (%s) but is NOT declared in %s as a 'License File', 'Source File', or 'Non-License File'", strings.Join(unexpectedMatches, ", "), readmePath)
	}

	return nil
}
