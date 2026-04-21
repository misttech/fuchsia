// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
)

type AllowlistCommand struct {
	fuchsiaDir string
}

func (*AllowlistCommand) Name() string     { return "allowlist" }
func (*AllowlistCommand) Synopsis() string { return "Manage allowed licenses." }
func (*AllowlistCommand) Usage() string {
	return `allowlist add <LicenseName> <projectPath>:
  Adds an allowed license exception for the given project path.

  Examples:
    fx check-licenses allowlist add GPL-2.0 vendor/foo
`
}

func (c *AllowlistCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
}

func (c *AllowlistCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 3 || f.Arg(0) != "add" {
		fmt.Fprintln(os.Stderr, "Usage: fx check-licenses allowlist add <LicenseName> <projectPath>")
		return subcommands.ExitUsageError
	}

	licenseName := f.Arg(1)
	projectPath := filepath.Clean(f.Arg(2))

	if c.fuchsiaDir == "" {
		c.fuchsiaDir = "."
	}
	absFuchsiaDir, err := filepath.Abs(c.fuchsiaDir)
	if err == nil {
		c.fuchsiaDir = absFuchsiaDir
	}

	// Make sure the project path is relative to FuchsiaDir
	if filepath.IsAbs(projectPath) {
		rel, err := filepath.Rel(c.fuchsiaDir, projectPath)
		if err == nil && !strings.HasPrefix(rel, "..") {
			projectPath = rel
		}
	}

	// Check if this project already has an exception for this license
	builder := v2config.NewBuilder(c.fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Warning: failed to assemble config: %v\n", err)
	} else {
		if list, ok := builder.Config.AllowedLicenses[licenseName]; ok {
			if _, exists := list[projectPath]; exists {
				fmt.Printf("Project '%s' already has an allowlist entry for '%s'. Nothing to do.\n", projectPath, licenseName)
				return subcommands.ExitSuccess
			}
		}
	}

	// Determine if this is a private project
	isPrivate := false
	if strings.HasPrefix(projectPath, "vendor/") {
		isPrivate = true
	} else if builder.Config != nil {
		if physicalPath, ok := builder.Config.OutOfTreeReadmes[projectPath]; ok {
			if strings.Contains(filepath.ToSlash(physicalPath), "/vendor/") {
				isPrivate = true
			}
		}
	}

	// Find the license category by scanning both public and private allowed_licenses dirs
	category := findLicenseCategory(c.fuchsiaDir, licenseName)

	configDir := filepath.Join(c.fuchsiaDir, "tools", "check-licenses", "assets", "configs", "allowed_licenses", category, licenseName)
	if isPrivate {
		configDir = filepath.Join(c.fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "allowed_licenses", category, licenseName)
	}

	if err := os.MkdirAll(configDir, 0755); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to create config directory %s: %v\n", configDir, err)
		return subcommands.ExitFailure
	}

	baseName := filepath.Base(projectPath)
	if baseName == "." || baseName == "/" {
		baseName = "root"
	}
	destFile := filepath.Join(configDir, baseName+".json")

	var cfg v2config.ConfigFile
	if data, err := os.ReadFile(destFile); err == nil {
		json.Unmarshal(data, &cfg)
	}

	if cfg.AllowedLicenses == nil {
		cfg.AllowedLicenses = make(map[string][]v2config.AllowlistEntry)
	}

	entry := v2config.AllowlistEntry{
		Bug:         "TODO: File bug with TQ-OSRB",
		Description: "Auto-generated allowlist entry",
		Paths:       []string{projectPath},
	}
	cfg.AllowedLicenses[licenseName] = append(cfg.AllowedLicenses[licenseName], entry)

	outData, err := json.MarshalIndent(cfg, "", "    ")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to marshal JSON: %v\n", err)
		return subcommands.ExitFailure
	}
	outData = append(outData, '\n')

	if err := os.WriteFile(destFile, outData, 0644); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to write config file %s: %v\n", destFile, err)
		return subcommands.ExitFailure
	}

	fmt.Printf("✅ Added Allowlist Entry:\n")
	fmt.Printf("  - License: %s\n", licenseName)
	fmt.Printf("  - Project: %s\n", projectPath)
	fmt.Printf("  - File:    %s\n\n", destFile)
	fmt.Printf("ACTION REQUIRED:\n")
	fmt.Printf("You must file a bug with the TQ-OSRB component explaining why this project needs to use this license.\n")
	fmt.Printf("Once filed, update the 'bug' field in %s with the bug number.\n", destFile)

	return subcommands.ExitSuccess
}

func findLicenseCategory(fuchsiaDir, licenseName string) string {
	dirsToSearch := []string{
		filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "configs", "allowed_licenses"),
		filepath.Join(fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "allowed_licenses"),
	}

	for _, baseDir := range dirsToSearch {
		if _, err := os.Stat(baseDir); os.IsNotExist(err) {
			continue
		}

		category := ""
		filepath.WalkDir(baseDir, func(path string, d fs.DirEntry, err error) error {
			if err != nil || !d.IsDir() {
				return nil
			}
			if d.Name() == licenseName {
				parent := filepath.Dir(path)
				category = filepath.Base(parent)
				return filepath.SkipDir // found it, stop walking this branch
			}
			return nil
		})

		if category != "" && category != "allowed_licenses" {
			return category
		}
	}

	return "Uncategorized"
}
