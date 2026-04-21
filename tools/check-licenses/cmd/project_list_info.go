// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

func (c *ProjectCommand) executeList(ctx context.Context, args []string, config *v2config.MasterConfig) subcommands.ExitStatus {
	dir := args[0]
	absDir, err := filepath.Abs(dir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to resolve absolute path: %v\n", err)
		return subcommands.ExitFailure
	}

	filepath.WalkDir(absDir, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			return nil
		}
		if filepath.Base(path) == "README.fuchsia" {
			readmes, _ := readme.ParseFile(path)
			for _, r := range readmes {
				rel, _ := filepath.Rel(c.fuchsiaDir, filepath.Dir(path))
				if r.Location != "" {
					rel = filepath.Join(rel, r.Location)
				}
				name := r.Name
				if name == "" {
					name = "Unknown Project"
				}
				fmt.Printf("//%s: %s\n", rel, name)
			}
		}
		return nil
	})

	for logicalPath, physicalPath := range config.OutOfTreeReadmes {
		absLogicalPath := filepath.Join(c.fuchsiaDir, logicalPath)
		if strings.HasPrefix(absLogicalPath, absDir) || absLogicalPath == absDir {
			readmes, _ := readme.ParseFile(physicalPath)
			for _, r := range readmes {
				rel := logicalPath
				if r.Location != "" {
					rel = filepath.Join(rel, r.Location)
				}
				name := r.Name
				if name == "" {
					name = "Unknown Project"
				}
				fmt.Printf("//%s: %s\n", rel, name)
			}
		}
	}

	return subcommands.ExitSuccess
}

func (c *ProjectCommand) executeInfo(ctx context.Context, args []string, config *v2config.MasterConfig) subcommands.ExitStatus {
	target := args[0]
	absTarget, err := filepath.Abs(target)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to resolve absolute path: %v\n", err)
		return subcommands.ExitFailure
	}

	r, readmePath, err := readme.FindProjectReadme(absTarget, c.fuchsiaDir, config.OutOfTreeReadmes)
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
			logicalRoot = filepath.Join(c.fuchsiaDir, logPath)
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

	relRoot, _ := filepath.Rel(c.fuchsiaDir, logicalRoot)

	fmt.Printf("Name:         %s\n", r.Name)
	fmt.Printf("URL:          %s\n", r.URL)
	fmt.Printf("Project Root: //%s\n", relRoot)

	virtualStr := ""
	if isVirtual {
		virtualStr = " (Virtual)"
	}
	relReadme, _ := filepath.Rel(c.fuchsiaDir, readmePath)
	fmt.Printf("Readme Path:  %s%s\n", relReadme, virtualStr)

	activePolicies := make(map[string]v2config.RuleMetadata)
	var activePolicyNames []string
	for policyName, paths := range config.PolicyExceptions {
		for p, meta := range paths {
			cleanP := strings.TrimPrefix(p, "//")
			if cleanP == relRoot || strings.HasPrefix(cleanP, relRoot+string(filepath.Separator)) || strings.HasPrefix(relRoot, cleanP+string(filepath.Separator)) {
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
			if cleanP == relRoot || strings.HasPrefix(cleanP, relRoot+string(filepath.Separator)) || strings.HasPrefix(relRoot, cleanP+string(filepath.Separator)) {
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
