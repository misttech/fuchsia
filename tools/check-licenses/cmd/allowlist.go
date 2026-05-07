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
	fuchsiaDir  string
	bug         string
	description string
}

func (*AllowlistCommand) Name() string     { return "allowlist" }
func (*AllowlistCommand) Synopsis() string { return "Manage allowed licenses." }
func (*AllowlistCommand) Usage() string {
	return `allowlist -bug <BugID> [-desc <Description>] add <LicenseName> <projectPath>:
  Adds an allowed license exception for the given project path.

  Flags:
    -bug  Bug ID tracking this exception (Mandatory).
    -desc Optional description for this exception.

  Examples:
    fx check-licenses allowlist -bug b/123 add GPL-2.0 vendor/foo
`
}

func (c *AllowlistCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
	f.StringVar(&c.bug, "bug", "", "Bug ID tracking this exception (Mandatory).")
	f.StringVar(&c.description, "desc", "Auto-generated allowlist entry", "Optional description for this exception.")
}

func (c *AllowlistCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 3 || f.Arg(0) != "add" {
		fmt.Fprintln(os.Stderr, "Usage: fx check-licenses allowlist -bug <BugID> [-desc <Description>] add <LicenseName> <projectPath>")
		return subcommands.ExitUsageError
	}

	if c.bug == "" {
		fmt.Fprintln(os.Stderr, "Error: the -bug flag is mandatory.")
		return subcommands.ExitUsageError
	}

	licenseName := f.Arg(1)
	projectPath := filepath.Clean(f.Arg(2))

	if err := AddAllowlistEntry(c.fuchsiaDir, licenseName, projectPath, c.bug, c.description); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}

// AddAllowlistEntry adds an allowed license exception for a given project path.
func AddAllowlistEntry(fuchsiaDir, licenseName, projectPath, bug, description string) error {
	if fuchsiaDir == "" {
		fuchsiaDir = "."
	}
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	// Resolve to absolute path first (handles relative to CWD)
	absProjectPath, err := filepath.Abs(projectPath)
	if err != nil {
		return fmt.Errorf("failed to get absolute path for %s: %w", projectPath, err)
	}

	// Make sure the project path is relative to FuchsiaDir
	rel, err := filepath.Rel(fuchsiaDir, absProjectPath)
	if err != nil || strings.HasPrefix(rel, "..") {
		return fmt.Errorf("project path %s must be inside fuchsia root %s", projectPath, fuchsiaDir)
	}
	projectPath = rel

	// Check if this project already has an exception for this license
	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		return fmt.Errorf("failed to assemble config: %w", err)
	}
	if list, ok := builder.Config.AllowedLicenses[licenseName]; ok {
		if _, exists := list[projectPath]; exists {
			fmt.Printf("Project '%s' already has an allowlist entry for '%s'. Nothing to do.\n", projectPath, licenseName)
			return nil
		}
	}

	// Determine if this is a private project
	isPrivate := false
	if builder.Config != nil {
		isPrivate = builder.Config.IsPrivateProject(projectPath)
	} else if strings.HasPrefix(projectPath, "vendor/") {
		isPrivate = true
	}

	// Find the license category by scanning both public and private allowed_licenses dirs
	category := findLicenseCategory(fuchsiaDir, licenseName)

	configDir := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "configs", "allowed_licenses", category, licenseName)
	if isPrivate {
		configDir = filepath.Join(fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "allowed_licenses", category, licenseName)
	}

	if err := os.MkdirAll(configDir, 0755); err != nil {
		return fmt.Errorf("failed to create config directory %s: %w", configDir, err)
	}

	baseName := findProjectBasename(projectPath, builder.Config.ManifestProjectNames)
	destFile := filepath.Join(configDir, baseName+".json")

	var cfg v2config.ConfigFile
	if data, err := os.ReadFile(destFile); err == nil {
		json.Unmarshal(data, &cfg)
	}

	if cfg.AllowedLicenses == nil {
		cfg.AllowedLicenses = make(map[string][]v2config.AllowlistEntry)
	}

	entry := v2config.AllowlistEntry{
		Bug:         bug,
		Description: description,
		Paths:       []string{projectPath},
	}
	cfg.AllowedLicenses[licenseName] = append(cfg.AllowedLicenses[licenseName], entry)

	outData, err := json.MarshalIndent(cfg, "", "    ")
	if err != nil {
		return fmt.Errorf("failed to marshal JSON: %w", err)
	}
	outData = append(outData, '\n')

	if err := os.WriteFile(destFile, outData, 0644); err != nil {
		return fmt.Errorf("failed to write config file %s: %w", destFile, err)
	}

	fmt.Printf("✅ Added Allowlist Entry:\n")
	fmt.Printf("  - License: %s\n", licenseName)
	fmt.Printf("  - Project: %s\n", projectPath)
	fmt.Printf("  - Bug:     %s\n", bug)
	fmt.Printf("  - File:    %s\n\n", destFile)

	return nil
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
