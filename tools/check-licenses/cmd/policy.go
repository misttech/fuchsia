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
	return `policy <subcommand> [options]:
  Manage policy exceptions.

  Subcommands:
    add   Add a new policy exception.
`
}

func (p *PolicyCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
}

func (p *PolicyCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("policy", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "policy")
	subCommander.Register(&PolicyAddCommand{fuchsiaDir: p.fuchsiaDir}, "")
	return subCommander.Execute(ctx)
}

type PolicyAddCommand struct {
	fuchsiaDir  string
	bug         string
	description string
}

func (*PolicyAddCommand) Name() string     { return "add" }
func (*PolicyAddCommand) Synopsis() string { return "Add a policy exception." }
func (*PolicyAddCommand) Usage() string {
	return `add -bug <BugID> [-desc <Description>] <CheckName> <targetPath>:
  Adds a policy exception for the given project or file path.

  Flags:
    -bug  Bug ID tracking this exception (Mandatory).
    -desc Optional description for this exception.

  Examples:
    fx check-licenses policy add -bug b/123 AllProjectsMustHaveALicense vendor/foo
    fx check-licenses policy add -bug b/456 -desc "Custom exception" AllLicenseTextsMustBeRecognized third_party/bar/LICENSE
`
}

func (p *PolicyAddCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.bug, "bug", "", "Bug ID tracking this exception (Mandatory).")
	f.StringVar(&p.description, "desc", "Auto-generated exception", "Optional description for this exception.")
}

func (p *PolicyAddCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
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

		if p.bug != "" && bugVal == "" {
			bugVal = p.bug
		}
		if p.description != "Auto-generated exception" && descVal == "" {
			descVal = p.description
		}

		var cmdBuilder strings.Builder
		cmdBuilder.WriteString("fx check-licenses policy add")
		if bugVal != "" {
			cmdBuilder.WriteString(fmt.Sprintf(" -bug %s", bugVal))
		} else {
			cmdBuilder.WriteString(" -bug <BugID>")
		}
		if descVal != "" && descVal != "Auto-generated exception" {
			cmdBuilder.WriteString(fmt.Sprintf(" -desc %q", descVal))
		}

		if len(positionals) > 0 {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", positionals[0]))
		} else {
			cmdBuilder.WriteString(" <CheckName>")
		}
		if len(positionals) > 1 {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", positionals[1]))
		} else {
			cmdBuilder.WriteString(" <targetPath>")
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

	if p.bug == "" {
		fmt.Fprintln(os.Stderr, "Error: the -bug flag is mandatory.")
		return subcommands.ExitUsageError
	}

	checkName := f.Arg(0)
	if !v2config.ValidPolicyChecks[checkName] {
		var validChecks []string
		for k := range v2config.ValidPolicyChecks {
			validChecks = append(validChecks, k)
		}
		fmt.Fprintf(os.Stderr, "Error: invalid check name %q. Must be one of: %s\n", checkName, strings.Join(validChecks, ", "))
		return subcommands.ExitUsageError
	}
	targetPath := filepath.Clean(f.Arg(1))

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
