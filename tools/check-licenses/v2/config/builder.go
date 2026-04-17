// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package config

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

// Builder handles the Assembly Phase, scanning directories for config files
// and aggregating them into a MasterConfig.
type Builder struct {
	FuchsiaDir string
	Config     *MasterConfig
}

// NewBuilder creates a new config assembler.
func NewBuilder(fuchsiaDir string) *Builder {
	return &Builder{
		FuchsiaDir: fuchsiaDir,
		Config:     NewMasterConfig(),
	}
}

// Assemble walks the standard open-source and proprietary vendor paths,
// merging any configuration JSON files it finds into the internal MasterConfig.
func (b *Builder) Assemble() error {
	// Standard open-source assets
	osAssets := filepath.Join(b.FuchsiaDir, "tools", "check-licenses", "assets")
	if err := b.walkAssetDir(osAssets); err != nil {
		return fmt.Errorf("failed to assemble open-source configs: %w", err)
	}

	// Proprietary vendor assets (may not exist in all checkouts)
	vendorAssets := filepath.Join(b.FuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets")
	if _, err := os.Stat(vendorAssets); err == nil {
		if err := b.walkAssetDir(vendorAssets); err != nil {
			return fmt.Errorf("failed to assemble vendor configs: %w", err)
		}
	}

	return nil
}

func (b *Builder) walkAssetDir(baseDir string) error {
	return filepath.WalkDir(baseDir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return err // Cannot access path
		}
		if d.IsDir() {
			return nil // Skip directories
		}

		relPath, err := filepath.Rel(baseDir, path)
		if err != nil {
			return err
		}
		parts := strings.Split(relPath, string(os.PathSeparator))
		if len(parts) < 2 {
			return nil // File directly in assets/, skip for now
		}

		category := parts[0]

		if category == "readmes" && filepath.Base(path) == "README.fuchsia" {
			if len(parts) > 2 {
				logicalParts := parts[1 : len(parts)-1] // strip "readmes" and "README.fuchsia"
				logicalPath := filepath.Join(logicalParts...)
				b.Config.OutOfTreeReadmes[logicalPath] = path
			}
			return nil
		}

		if category == "configs" && filepath.Ext(path) == ".json" {
			if filepath.Base(path) == "template.json" {
				return nil // Never parse template files
			}
			return b.parseConfigFile(path)
		}

		return nil
	})
}

func (b *Builder) parseConfigFile(path string) error {
	bytes, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	var f ConfigFile
	if err := json.Unmarshal(bytes, &f); err != nil {
		return fmt.Errorf("failed to parse config file %q: %w", path, err)
	}

	// 1. Process Skips
	for _, skip := range f.Skips {
		if skip.Bug == "" && filepath.Base(path) != "default.json" && filepath.Base(path) != "hidden_dirs.json" && filepath.Base(path) != "test_dirs.json" && filepath.Base(path) != "bazel_vendor.json" {
			return fmt.Errorf("validation error in %s: a 'bug' field is required to track this exception", path)
		}
		if skip.SkipAnywhere {
			b.Config.SkipAnywhere = append(b.Config.SkipAnywhere, skip.Paths...)
		} else {
			b.Config.SkipPaths = append(b.Config.SkipPaths, skip.Paths...)
		}
	}

	// 2. Process TargetExtensions
	if f.TargetExtensions != nil {
		for _, ext := range f.TargetExtensions.Extensions {
			// Ensure consistent dot formatting (e.g., "cc" -> ".cc")
			if !strings.HasPrefix(ext, ".") {
				ext = "." + ext
			}
			b.Config.TargetExtensions[ext] = true
		}
	}

	// 3. Process Barriers
	for _, barrier := range f.Barriers {
		if barrier.Bug == "" && filepath.Base(path) != "default.json" && filepath.Base(path) != "hidden_dirs.json" && filepath.Base(path) != "test_dirs.json" && filepath.Base(path) != "bazel_vendor.json" {
			return fmt.Errorf("validation error in %s: a 'bug' field is required to track this exception", path)
		}
		b.Config.BarrierPaths = append(b.Config.BarrierPaths, barrier.Paths...)
	}

	// 4. Process PolicyExceptions
	for checkName, entries := range f.PolicyExceptions {
		if _, exists := b.Config.PolicyExceptions[checkName]; !exists {
			b.Config.PolicyExceptions[checkName] = make(map[string]bool)
		}
		for _, entry := range entries {
			if entry.Bug == "" && filepath.Base(path) != "default.json" && filepath.Base(path) != "hidden_dirs.json" && filepath.Base(path) != "test_dirs.json" && filepath.Base(path) != "bazel_vendor.json" {
				return fmt.Errorf("validation error in %s: a 'bug' field is required to track this exception", path)
			}
			for _, allowedPath := range entry.Paths {
				b.Config.PolicyExceptions[checkName][allowedPath] = true
			}
		}
	}

	// 5. Process AllowedLicenses
	for licenseName, entries := range f.AllowedLicenses {
		if _, exists := b.Config.AllowedLicenses[licenseName]; !exists {
			b.Config.AllowedLicenses[licenseName] = make(map[string]bool)
		}
		for _, entry := range entries {
			if entry.Bug == "" && filepath.Base(path) != "default.json" && filepath.Base(path) != "hidden_dirs.json" && filepath.Base(path) != "test_dirs.json" && filepath.Base(path) != "bazel_vendor.json" {
				return fmt.Errorf("validation error in %s: a 'bug' field is required to track this exception", path)
			}
			for _, allowedPath := range entry.Paths {
				b.Config.AllowedLicenses[licenseName][allowedPath] = true
			}
		}
	}

	return nil
}
