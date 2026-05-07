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
	return `allowlist <subcommand> [options]:
  Manage allowed licenses.

  Subcommands:
    add   Add a new allowed license entry.
`
}

func (c *AllowlistCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
}

func (c *AllowlistCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("allowlist", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "allowlist")
	subCommander.Register(&AllowlistAddCommand{fuchsiaDir: c.fuchsiaDir}, "")
	return subCommander.Execute(ctx)
}

type AllowlistAddCommand struct {
	fuchsiaDir  string
	bug         string
	description string
}

func (*AllowlistAddCommand) Name() string     { return "add" }
func (*AllowlistAddCommand) Synopsis() string { return "Add an allowed license entry." }
func (*AllowlistAddCommand) Usage() string {
	return `add -bug <BugID> [-desc <Description>] add <LicenseName> <projectPath>:
  Adds an allowed license exception for the given project path.

  Flags:
    -bug  Bug ID tracking this exception (Mandatory).
    -desc Optional description for this exception.

  Examples:
    fx check-licenses allowlist add -bug b/123 GPL-2.0 vendor/foo
`
}

func (c *AllowlistAddCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.bug, "bug", "", "Bug ID tracking this exception (Mandatory).")
	f.StringVar(&c.description, "desc", "Auto-generated allowlist entry", "Optional description for this exception.")
}

func (c *AllowlistAddCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	// UX Check: Detect misplaced flags
	misplacedFlags := false
	for _, arg := range f.Args() {
		if strings.HasPrefix(arg, "-") {
			misplacedFlags = true
			break
		}
	}

	if misplacedFlags || f.NArg() != 2 {
		var bugVal string
		var descVal string
		var positionals []string

		args := f.Args()
		for i := 0; i < len(args); i++ {
			arg := args[i]
			if arg == "-bug" || arg == "--bug" {
				if i+1 < len(args) {
					bugVal = args[i+1]
					i++
				}
			} else if arg == "-desc" || arg == "--desc" {
				if i+1 < len(args) {
					descVal = args[i+1]
					i++
				}
			} else if strings.HasPrefix(arg, "-") {
				// skip unknown flags
			} else {
				positionals = append(positionals, arg)
			}
		}

		if c.bug != "" && bugVal == "" {
			bugVal = c.bug
		}
		if c.description != "Auto-generated allowlist entry" && descVal == "" {
			descVal = c.description
		}

		var cmdBuilder strings.Builder
		cmdBuilder.WriteString("fx check-licenses allowlist add")
		if bugVal != "" {
			cmdBuilder.WriteString(fmt.Sprintf(" -bug %s", bugVal))
		} else {
			cmdBuilder.WriteString(" -bug <BugID>")
		}
		if descVal != "" && descVal != "Auto-generated allowlist entry" {
			cmdBuilder.WriteString(fmt.Sprintf(" -desc %q", descVal))
		}

		if len(positionals) > 0 {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", positionals[0]))
		} else {
			cmdBuilder.WriteString(" <LicenseName>")
		}
		if len(positionals) > 1 {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", positionals[1]))
		} else {
			cmdBuilder.WriteString(" <projectPath>")
		}
		for _, extra := range positionals[2:] {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", extra))
		}

		if misplacedFlags {
			fmt.Fprintln(os.Stderr, "❌ Error: Flags (like -bug or -desc) must be placed BEFORE positional arguments.")
		} else {
			fmt.Fprintln(os.Stderr, "❌ Error: Invalid number of arguments.")
		}
		fmt.Fprintf(os.Stderr, "Try running this copy-pasteable command instead:\n    %s\n\n", cmdBuilder.String())
		return subcommands.ExitUsageError
	}

	if c.bug == "" {
		fmt.Fprintln(os.Stderr, "Error: the -bug flag is mandatory.")
		return subcommands.ExitUsageError
	}

	licenseName := f.Arg(0)
	projectPath := filepath.Clean(f.Arg(1))

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
