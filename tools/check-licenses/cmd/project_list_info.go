// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

type ProjectListCommand struct {
	fuchsiaDir string
}

func (*ProjectListCommand) Name() string { return "list" }
func (*ProjectListCommand) Synopsis() string {
	return "Lists all project boundaries discovered under the directory."
}
func (*ProjectListCommand) Usage() string {
	return `list <dir>:
  Lists all project boundaries discovered under the given directory.
`
}

func (c *ProjectListCommand) SetFlags(f *flag.FlagSet) {}

func (c *ProjectListCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	dir := c.fuchsiaDir
	if f.NArg() > 0 {
		dir = f.Arg(0)
	}
	fuchsiaDir, targetDir, err := ResolveAndValidatePath(c.fuchsiaDir, dir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}
	absDir := filepath.Join(fuchsiaDir, targetDir)

	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	projects, err := readme.DiscoverProjects(absDir, fuchsiaDir, config)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to discover projects: %v\n", err)
		return subcommands.ExitFailure
	}

	// Also add out-of-tree readmes!
	for logicalPath, physicalPath := range config.OutOfTreeReadmes {
		absLogicalPath := filepath.Join(fuchsiaDir, logicalPath)
		if strings.HasPrefix(absLogicalPath, absDir) || absLogicalPath == absDir {
			readmes, _ := readme.ParseFile(physicalPath)
			for _, r := range readmes {
				rel := logicalPath
				if r.Location != "" && r.Location != "." {
					rel = filepath.Join(rel, r.Location)
				}
				name := r.Name
				if name == "" {
					name = "Unknown Project"
				}
				projects = append(projects, readme.ProjectInfo{
					Path: rel,
					Name: name,
				})
			}
		}
	}

	// Sort all projects by path for consistent output!
	sort.Slice(projects, func(i, j int) bool {
		return projects[i].Path < projects[j].Path
	})

	for _, p := range projects {
		fmt.Printf("//%s: %s\n", p.Path, p.Name)
	}

	return subcommands.ExitSuccess
}

type ProjectInfoCommand struct {
	fuchsiaDir string
}

func (*ProjectInfoCommand) Name() string { return "info" }
func (*ProjectInfoCommand) Synopsis() string {
	return "Shows the project metadata and compliance state for a given file or directory."
}
func (*ProjectInfoCommand) Usage() string {
	return `info <path>:
  Shows the project metadata and compliance state for a given file or directory.
`
}

func (c *ProjectInfoCommand) SetFlags(f *flag.FlagSet) {}

func (c *ProjectInfoCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one path must be provided.")
		return subcommands.ExitUsageError
	}

	target := f.Arg(0)
	fuchsiaDir, targetPath, err := ResolveAndValidatePath(c.fuchsiaDir, target)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}
	absTarget := filepath.Join(fuchsiaDir, targetPath)

	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	r, readmePath, err := readme.FindProjectReadme(absTarget, fuchsiaDir, config.OutOfTreeReadmes)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to find project README: %v\n", err)
		return subcommands.ExitFailure
	}
	if r == nil {
		fmt.Fprintf(os.Stderr, "No project README found for %s\n", target)
		return subcommands.ExitFailure
	}

	var logicalRoot string
	isVirtual := false
	for logPath, physPath := range config.OutOfTreeReadmes {
		if physPath == readmePath {
			logicalRoot = filepath.Join(fuchsiaDir, logPath)
			isVirtual = true
			break
		}
	}
	if logicalRoot == "" {
		logicalRoot = filepath.Dir(readmePath)
	}

	if r.Location != "" {
		logicalRoot = filepath.Join(logicalRoot, r.Location)
	}

	relRoot, _ := filepath.Rel(fuchsiaDir, logicalRoot)

	fmt.Printf("Name:         %s\n", r.Name)
	fmt.Printf("URL:          %s\n", r.URL)
	fmt.Printf("Project Root: //%s\n", relRoot)

	virtualStr := ""
	if isVirtual {
		virtualStr = " (Virtual)"
	}
	relReadme, _ := filepath.Rel(fuchsiaDir, readmePath)
	fmt.Printf("Readme Path:  %s%s\n", relReadme, virtualStr)

	readmeCache := make(map[string]*readmeResult)
	activePolicies := make(map[string]v2config.RuleMetadata)
	var activePolicyNames []string
	for policyName, paths := range config.PolicyExceptions {
		for p, meta := range paths {
			cleanP := strings.TrimPrefix(p, "//")
			if belongsToProject(cleanP, relRoot, fuchsiaDir, config.OutOfTreeReadmes, readmeCache) {
				activePolicies[policyName] = meta
				activePolicyNames = append(activePolicyNames, policyName)
				break
			}
		}
	}
	sort.Strings(activePolicyNames)

	allowedLicenses := make(map[string]v2config.RuleMetadata)
	var allowedLicenseNames []string
	for licenseID, paths := range config.AllowedLicenses {
		for p, meta := range paths {
			cleanP := strings.TrimPrefix(p, "//")
			if belongsToProject(cleanP, relRoot, fuchsiaDir, config.OutOfTreeReadmes, readmeCache) {
				allowedLicenses[licenseID] = meta
				allowedLicenseNames = append(allowedLicenseNames, licenseID)
				break
			}
		}
	}
	sort.Strings(allowedLicenseNames)

	if len(activePolicyNames) > 0 {
		fmt.Println("\nPolicy Overrides:")
		for _, name := range activePolicyNames {
			meta := activePolicies[name]
			fmt.Printf("  - %s\n", name)
			if meta.ConfigPath != "" {
				relConfig, _ := filepath.Rel(fuchsiaDir, meta.ConfigPath)
				fmt.Printf("      Config: %s\n", relConfig)
			}
			if meta.Bug != "" {
				fmt.Printf("      Bug: %s\n", meta.Bug)
			}
			if meta.Description != "" {
				fmt.Printf("      Description: %s\n", meta.Description)
			}
		}
	}

	if len(allowedLicenseNames) > 0 {
		fmt.Println("\nAllowed Licenses:")
		for _, name := range allowedLicenseNames {
			meta := allowedLicenses[name]
			fmt.Printf("  - %s\n", name)
			if meta.ConfigPath != "" {
				relConfig, _ := filepath.Rel(fuchsiaDir, meta.ConfigPath)
				fmt.Printf("      Config: %s\n", relConfig)
			}
			if meta.Bug != "" {
				fmt.Printf("      Bug: %s\n", meta.Bug)
			}
			if meta.Description != "" {
				fmt.Printf("      Description: %s\n", meta.Description)
			}
		}
	}
	fmt.Println()

	fmt.Println("--- Parsed README.fuchsia Content ---")
	fmt.Println(readme.Format([]*readme.Readme{r}))

	return subcommands.ExitSuccess
}

type readmeResult struct {
	r    *readme.Readme
	path string
	err  error
}

func belongsToProject(policyPath string, projectRoot string, fuchsiaDir string, outOfTreeReadmes map[string]string, cache map[string]*readmeResult) bool {
	// If the policy applies to a broader root that contains this project, we inherit it.
	// E.g. Policy on "third_party" applies to "third_party/foo".
	if strings.HasPrefix(projectRoot, policyPath+string(filepath.Separator)) {
		return true
	}

	// If it's an exact match, obviously true.
	if policyPath == projectRoot {
		return true
	}

	// If the policy path is outside our project hierarchy, false.
	if !strings.HasPrefix(policyPath, projectRoot+string(filepath.Separator)) {
		return false
	}

	// The policy path is SUBORDINATE to our project root (e.g. policy on "src/foo/bar" and we are querying "src/foo").
	// We only claim this policy if "src/foo/bar" is actually part of our project, NOT a separate sub-project.
	absPolicyPath := filepath.Join(fuchsiaDir, policyPath)

	var res *readmeResult
	var ok bool
	if res, ok = cache[absPolicyPath]; !ok {
		r, readmePath, err := readme.FindProjectReadme(absPolicyPath, fuchsiaDir, outOfTreeReadmes)
		res = &readmeResult{r: r, path: readmePath, err: err}
		cache[absPolicyPath] = res
	}

	if res.err != nil || res.r == nil {
		// If we can't find a boundary, default to naive prefix matching (it belongs to us)
		return true
	}

	var pLogicalRoot string
	for logPath, physPath := range outOfTreeReadmes {
		if physPath == res.path {
			pLogicalRoot = filepath.Join(fuchsiaDir, logPath)
			break
		}
	}
	if pLogicalRoot == "" {
		pLogicalRoot = filepath.Dir(res.path)
	}

	if res.r.Location != "" {
		pLogicalRoot = filepath.Join(pLogicalRoot, res.r.Location)
	}

	pRelRoot, _ := filepath.Rel(fuchsiaDir, pLogicalRoot)

	// It belongs to us only if the closest project boundary to the policy file is US.
	return pRelRoot == projectRoot
}
