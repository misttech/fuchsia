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
	fuchsiaDir  string
	bug         string
	description string
}

func (*PolicyCommand) Name() string     { return "policy" }
func (*PolicyCommand) Synopsis() string { return "Manage policy exceptions." }
func (*PolicyCommand) Usage() string {
	return `policy add <CheckName> <targetPath> -bug <BugID> [-desc <Description>]:
  Adds a policy exception for the given project or file path.

  Flags:
    -bug  Bug ID tracking this exception (Mandatory).
    -desc Optional description for this exception.

  Examples:
    fx check-licenses policy -bug b/123 add AllProjectsMustHaveALicense vendor/foo
    fx check-licenses policy -bug b/456 -desc "Custom exception" add AllLicenseTextsMustBeRecognized third_party/bar/LICENSE

`
}

func (p *PolicyCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
	f.StringVar(&p.bug, "bug", "", "Bug ID tracking this exception (Mandatory).")
	f.StringVar(&p.description, "desc", "Auto-generated exception", "Optional description for this exception.")
}

func (p *PolicyCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 3 || f.Arg(0) != "add" {
		fmt.Fprintln(os.Stderr, "Usage: fx check-licenses policy -bug <BugID> [-desc <Description>] add <CheckName> <targetPath>")
		return subcommands.ExitUsageError
	}

	if p.bug == "" {
		fmt.Fprintln(os.Stderr, "Error: the -bug flag is mandatory.")
		return subcommands.ExitUsageError
	}

	checkName := f.Arg(1)
	if !v2config.ValidPolicyChecks[checkName] {
		var validChecks []string
		for k := range v2config.ValidPolicyChecks {
			validChecks = append(validChecks, k)
		}
		fmt.Fprintf(os.Stderr, "Error: invalid check name %q. Must be one of: %s\n", checkName, strings.Join(validChecks, ", "))
		return subcommands.ExitUsageError
	}
	targetPath := filepath.Clean(f.Arg(2))

	if err := AddPolicyException(p.fuchsiaDir, checkName, targetPath, p.bug, p.description); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}

// AddPolicyException adds a policy exception for a given project or file path.
func AddPolicyException(fuchsiaDir, checkName, targetPath, bug, description string) error {
	if fuchsiaDir == "" {
		fuchsiaDir = "."
	}
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	// Resolve to absolute path first (handles relative to CWD)
	absTargetPath, err := filepath.Abs(targetPath)
	if err != nil {
		return fmt.Errorf("failed to get absolute path for %s: %w", targetPath, err)
	}

	// Make sure the target path is relative to FuchsiaDir
	rel, err := filepath.Rel(fuchsiaDir, absTargetPath)
	if err != nil || strings.HasPrefix(rel, "..") {
		return fmt.Errorf("target path %s must be inside fuchsia root %s", targetPath, fuchsiaDir)
	}
	targetPath = rel

	// Check if this target already has an exception
	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		return fmt.Errorf("failed to assemble config: %w", err)
	}
	if list, ok := builder.Config.PolicyExceptions[checkName]; ok {
		if _, exists := list[targetPath]; exists {
			fmt.Printf("Path '%s' already has a policy exception for '%s'. Nothing to do.\n", targetPath, checkName)
			return nil
		}
	}

	// Determine if this is a private project
	isPrivate := false
	if builder.Config != nil {
		isPrivate = builder.Config.IsPrivateProject(targetPath)
	} else if strings.HasPrefix(targetPath, "vendor/") {
		isPrivate = true
	}

	configDir := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", checkName)
	if isPrivate {
		configDir = filepath.Join(fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "policy_exceptions", checkName)
	}

	if err := os.MkdirAll(configDir, 0755); err != nil {
		return fmt.Errorf("failed to create config directory %s: %w", configDir, err)
	}

	// Determine the config file name based on project name or top-level component
	baseName := findProjectBasename(targetPath, builder.Config.ManifestProjectNames)
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
		Bug:         bug,
		Description: description,
		Paths:       []string{targetPath},
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
	fmt.Printf("  - Check:  %s\n", checkName)
	fmt.Printf("  - Target: %s\n", targetPath)
	fmt.Printf("  - Bug:    %s\n", bug)
	fmt.Printf("  - File:   %s\n\n", destFile)

	return nil
}

func findProjectBasename(targetPath string, manifestProjectNames map[string]string) string {
	targetPath = filepath.Clean(targetPath)

	// Walk up the path to find a match in manifests
	for p := targetPath; p != "." && p != "/"; p = filepath.Dir(p) {
		if _, ok := manifestProjectNames[p]; ok {
			return filepath.Base(p)
		}
	}

	// Fallback for first-party or paths not in manifest
	dir := filepath.Dir(targetPath)
	if dir == "." || dir == "/" {
		return "root"
	}

	parts := strings.Split(targetPath, string(filepath.Separator))
	if len(parts) > 0 && parts[0] != "" {
		if parts[0] == "src" && len(parts) > 1 && parts[1] != "" {
			return parts[1]
		}
		return parts[0]
	}

	return "root"
}
