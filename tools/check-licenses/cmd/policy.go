// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
)

type PolicyCommand struct {
	fuchsiaDir string
}

func (*PolicyCommand) Name() string     { return "policy" }
func (*PolicyCommand) Synopsis() string { return "Manage policy exceptions." }
func (*PolicyCommand) Usage() string {
	return `policy add <CheckName> <projectPath>:
  Adds a policy exception for the given project path.

  Examples:
    fx check-licenses policy add AllProjectsMustHaveALicense vendor/foo

`
}

func (p *PolicyCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
}

func (p *PolicyCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 3 || f.Arg(0) != "add" {
		fmt.Fprintln(os.Stderr, "Usage: fx check-licenses policy add <CheckName> <projectPath>")
		return subcommands.ExitUsageError
	}

	checkName := f.Arg(1)
	projectPath := filepath.Clean(f.Arg(2))

	if err := AddPolicyException(p.fuchsiaDir, checkName, projectPath); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}

// AddPolicyException adds a policy exception for a given project path.
func AddPolicyException(fuchsiaDir, checkName, projectPath string) error {
	if fuchsiaDir == "" {
		fuchsiaDir = "."
	}
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	// Make sure the project path is relative to FuchsiaDir
	if filepath.IsAbs(projectPath) {
		rel, err := filepath.Rel(fuchsiaDir, projectPath)
		if err == nil && !strings.HasPrefix(rel, "..") {
			projectPath = rel
		}
	}

	// Check if this project already has an exception
	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		return fmt.Errorf("failed to assemble config: %w", err)
	}
	if list, ok := builder.Config.PolicyExceptions[checkName]; ok {
		if _, exists := list[projectPath]; exists {
			fmt.Printf("Project '%s' already has a policy exception for '%s'. Nothing to do.\n", projectPath, checkName)
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

	configDir := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", checkName)
	if isPrivate {
		configDir = filepath.Join(fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "policy_exceptions", checkName)
	}

	if err := os.MkdirAll(configDir, 0755); err != nil {
		return fmt.Errorf("failed to create config directory %s: %w", configDir, err)
	}

	// We'll write to a file named after the project base name
	baseName := filepath.Base(projectPath)
	if baseName == "." || baseName == "/" {
		baseName = "root"
	}
	destFile := filepath.Join(configDir, baseName+".json")

	// Read existing if any
	var cfg v2config.ConfigFile
	if data, err := os.ReadFile(destFile); err == nil {
		json.Unmarshal(data, &cfg)
	}

	if cfg.PolicyExceptions == nil {
		cfg.PolicyExceptions = make(map[string][]v2config.AllowlistEntry)
	}

	// Append
	entry := v2config.AllowlistEntry{
		Bug:         "TODO: File bug with TQ-OSRB",
		Description: "Auto-generated exception",
		Paths:       []string{projectPath},
	}
	cfg.PolicyExceptions[checkName] = append(cfg.PolicyExceptions[checkName], entry)

	// Write back
	outData, err := json.MarshalIndent(cfg, "", "    ")
	if err != nil {
		return fmt.Errorf("failed to marshal JSON: %w", err)
	}
	outData = append(outData, '\n') // POSIX standard

	if err := os.WriteFile(destFile, outData, 0644); err != nil {
		return fmt.Errorf("failed to write config file %s: %w", destFile, err)
	}

	fmt.Printf("✅ Added Policy Exception:\n")
	fmt.Printf("  - Check:   %s\n", checkName)
	fmt.Printf("  - Project: %s\n", projectPath)
	fmt.Printf("  - File:    %s\n\n", destFile)
	fmt.Printf("ACTION REQUIRED:\n")
	fmt.Printf("You must file a bug with the TQ-OSRB component explaining why this project needs a policy exception.\n")
	fmt.Printf("Once filed, update the 'bug' field in %s with the bug number.\n", destFile)

	return nil
}
