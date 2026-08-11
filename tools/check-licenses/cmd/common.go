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
	v2readme "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
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

func findProjectBasename(fuchsiaDir, targetPath string, config *v2config.MasterConfig) string {
	cleanTargetPath := filepath.Clean(targetPath)
	absTargetPath := filepath.Join(fuchsiaDir, cleanTargetPath)

	// Priority 1: Check README.fuchsia (or Cargo.toml / go.mod) for an explicit Name
	metadataFiles := []string{"README.fuchsia", "Cargo.toml", "go.mod", "pubspec.yaml"}
	for _, mf := range metadataFiles {
		metaPath := filepath.Join(absTargetPath, mf)
		if _, err := os.Stat(metaPath); err == nil {
			if rootReadmes, subReadmes, err := v2readme.ParseAnyMetadata(metaPath); err == nil {
				if len(rootReadmes) > 0 && rootReadmes[0].Name != "" {
					return filepath.Base(rootReadmes[0].Name)
				}
				if len(subReadmes) > 0 && subReadmes[0].Name != "" {
					return filepath.Base(subReadmes[0].Name)
				}
			}
		}
	}

	// Priority 1.5: Check Virtual Out-Of-Tree READMEs from MasterConfig
	if config != nil && config.Boundary.OutOfTreeReadmes != nil {
		if virtualReadmePath, ok := config.Boundary.OutOfTreeReadmes[cleanTargetPath]; ok {
			absVirtualPath := virtualReadmePath
			if !filepath.IsAbs(absVirtualPath) {
				absVirtualPath = filepath.Join(fuchsiaDir, virtualReadmePath)
			}
			if _, err := os.Stat(absVirtualPath); err == nil {
				if rootReadmes, _, err := v2readme.ParseAnyMetadata(absVirtualPath); err == nil && len(rootReadmes) > 0 && rootReadmes[0].Name != "" {
					return filepath.Base(rootReadmes[0].Name)
				}
			}
		}
	}

	// Priority 2: Check Jiri Manifest mapping
	if config != nil {
		if name := config.ManifestNameFor(cleanTargetPath); name != "" {
			return filepath.Base(name)
		}
	}

	// Priority 3: Fallback for first-party or paths not in manifest
	dir := filepath.Dir(cleanTargetPath)
	if dir == "." || dir == "/" {
		return "root"
	}

	parts := strings.Split(cleanTargetPath, string(filepath.Separator))
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

	var absInputPath string
	if filepath.IsAbs(inputPath) {
		absInputPath = filepath.Clean(inputPath)
	} else {
		if wd, err := os.Getwd(); err == nil && (wd == absFuchsiaDir || strings.HasPrefix(wd, absFuchsiaDir+string(filepath.Separator))) {
			absInputPath = filepath.Join(wd, inputPath)
		} else {
			absInputPath = filepath.Join(absFuchsiaDir, inputPath)
		}
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

// InputContext encapsulates the workspace root, resolved input paths, and assembled configuration.
type InputContext struct {
	FuchsiaDir string
	RelPath    string
	AbsPath    string
	Config     *v2config.MasterConfig
}

// LoadInputContext normalizes the input path within the Fuchsia workspace and loads the v2 MasterConfig.
func LoadInputContext(fuchsiaDirFlag, inputPath string) (*InputContext, error) {
	absFuchsia, relPath, err := ResolveAndValidatePath(fuchsiaDirFlag, inputPath)
	if err != nil {
		return nil, err
	}
	absPath := filepath.Join(absFuchsia, relPath)
	builder := v2config.NewBuilder(absFuchsia)
	if err := builder.Assemble(); err != nil {
		return nil, fmt.Errorf("failed to assemble configuration: %w", err)
	}
	return &InputContext{
		FuchsiaDir: absFuchsia,
		RelPath:    relPath,
		AbsPath:    absPath,
		Config:     builder.Config,
	}, nil
}

// ResolveProjectRoot resolves the governing logical project root directory for a given input path.
func (ic *InputContext) ResolveProjectRoot(inputPath string) (string, error) {
	fuchsiaDir, relPath, err := ResolveAndValidatePath(ic.FuchsiaDir, inputPath)
	if err != nil {
		return "", err
	}
	absPath := filepath.Join(fuchsiaDir, relPath)
	info, err := os.Stat(absPath)
	if err != nil {
		return "", fmt.Errorf("path does not exist: %s", inputPath)
	}
	r, bestReadmePath, err := v2readme.FindProjectReadme(absPath, fuchsiaDir, ic.Config.Boundary.OutOfTreeReadmes)
	if err == nil && bestReadmePath != "" && r != nil {
		return v2readme.ResolveProjectRoot(r, bestReadmePath, fuchsiaDir, ic.Config.Boundary.OutOfTreeReadmes), nil
	}
	if info.IsDir() {
		return absPath, nil
	}
	return filepath.Dir(absPath), nil
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

// LoadTargets parses and combines target paths from both the command line arguments and an optional file list.
func LoadTargets(fileList, fuchsiaDir string, args []string) ([]string, error) {
	var targets []string
	if fileList != "" {
		absList := fileList
		if !filepath.IsAbs(absList) {
			absList = filepath.Join(fuchsiaDir, fileList)
		}
		data, err := os.ReadFile(absList)
		if err != nil {
			return nil, fmt.Errorf("failed to read file-list %s: %w", fileList, err)
		}
		for _, line := range strings.Split(string(data), "\n") {
			line = strings.TrimSpace(line)
			if line != "" && !strings.HasPrefix(line, "#") {
				targets = append(targets, line)
			}
		}
	}
	targets = append(targets, args...)
	if len(targets) == 0 {
		return nil, fmt.Errorf("at least one target path must be provided via positional arguments or -file-list")
	}
	return targets, nil
}
