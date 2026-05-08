// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
)

// ReconstructCommand scans raw args to find -bug and -desc flags and their values,
// separates them from positional arguments, and formats a correct syntax suggestion string.
// Returns the formatted command string and true if any misplaced flags (starting with -) were found.
func ReconstructCommand(commandPath string, args []string, placeholders []string, activeBug, activeDesc string) (string, bool) {
	misplacedFlags := false
	for _, arg := range args {
		if strings.HasPrefix(arg, "-") {
			misplacedFlags = true
			break
		}
	}

	var bugVal string
	var descVal string
	var positionals []string

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

	if activeBug != "" && bugVal == "" {
		bugVal = activeBug
	}
	if activeDesc != "" && activeDesc != "Auto-generated exception" && activeDesc != "Auto-generated allowlist entry" && descVal == "" {
		descVal = activeDesc
	}

	var cmdBuilder strings.Builder
	cmdBuilder.WriteString(fmt.Sprintf("fx check-licenses %s", commandPath))
	if bugVal != "" {
		cmdBuilder.WriteString(fmt.Sprintf(" -bug %s", bugVal))
	} else {
		cmdBuilder.WriteString(" -bug <BugID>")
	}
	if descVal != "" && descVal != "Auto-generated exception" && descVal != "Auto-generated allowlist entry" {
		cmdBuilder.WriteString(fmt.Sprintf(" -desc %q", descVal))
	}

	for i, placeholder := range placeholders {
		if i < len(positionals) {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", positionals[i]))
		} else {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", placeholder))
		}
	}
	// Append remaining positionals if any
	if len(positionals) > len(placeholders) {
		for _, extra := range positionals[len(placeholders):] {
			cmdBuilder.WriteString(fmt.Sprintf(" %s", extra))
		}
	}

	return cmdBuilder.String(), misplacedFlags
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

// ResolveAndValidatePath normalizes the fuchsia root and ensures the given input path
// resides safely within that root. Returns the absolute fuchsia path, the relative target path,
// or an error if the path escapes the root workspace.
func ResolveAndValidatePath(fuchsiaDir, inputPath string) (string, string, error) {
	if fuchsiaDir == "" {
		fuchsiaDir = os.Getenv("FUCHSIA_DIR")
		if fuchsiaDir == "" {
			fuchsiaDir = "."
		}
	}
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err != nil {
		return "", "", fmt.Errorf("failed to get absolute path for fuchsia_dir %s: %w", fuchsiaDir, err)
	}

	absInputPath, err := filepath.Abs(inputPath)
	if err != nil {
		return "", "", fmt.Errorf("failed to get absolute path for %s: %w", inputPath, err)
	}

	rel, err := filepath.Rel(absFuchsiaDir, absInputPath)
	if err != nil || strings.HasPrefix(rel, "..") {
		return "", "", fmt.Errorf("path %s must be inside fuchsia root %s", inputPath, absFuchsiaDir)
	}
	if rel == "." {
		rel = ""
	}
	return absFuchsiaDir, rel, nil
}

// UpdateConfigFile reads, mutates, and writes back a ConfigFile.
func UpdateConfigFile(destFile string, mutate func(*v2config.ConfigFile)) error {
	var cfg v2config.ConfigFile
	if data, err := os.ReadFile(destFile); err == nil {
		json.Unmarshal(data, &cfg)
	}

	mutate(&cfg)

	outData, err := json.MarshalIndent(cfg, "", "    ")
	if err != nil {
		return fmt.Errorf("failed to marshal JSON: %w", err)
	}
	outData = append(outData, '\n') // POSIX standard

	if err := os.WriteFile(destFile, outData, 0644); err != nil {
		return fmt.Errorf("failed to write config file %s: %w", destFile, err)
	}
	return nil
}
