// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
)

const unknownTarget = "Unknown to Build"

// CompileCommand represents an entry in compile_commands.json.
type CompileCommand struct {
	Directory string   `json:"directory"`
	File      string   `json:"file"`
	Command   string   `json:"command,omitempty"`
	Arguments []string `json:"arguments,omitempty"`
}

// GetArgs returns the command line arguments for the compilation.
func (c *CompileCommand) GetArgs() []string {
	if len(c.Arguments) > 0 {
		return c.Arguments
	}
	return splitCommand(c.Command)
}

// splitCommand splits a shell command string into arguments, handling simple quoting.
func splitCommand(cmd string) []string {
	var args []string
	var arg strings.Builder
	inQuote := false
	var quoteChar rune

	runes := []rune(cmd)
	for i := 0; i < len(runes); i++ {
		r := runes[i]

		// Handle backslash escapes. Note that this simple implementation
		// treats backslash as an escape character both inside and outside quotes.
		if r == '\\' && i+1 < len(runes) {
			arg.WriteRune(runes[i+1])
			i++
			continue
		}

		if inQuote {
			if r == quoteChar {
				inQuote = false
			} else {
				arg.WriteRune(r)
			}
		} else {
			if r == '"' || r == '\'' {
				inQuote = true
				quoteChar = r
			} else if r == ' ' {
				if arg.Len() > 0 {
					args = append(args, arg.String())
					arg.Reset()
				}
			} else {
				arg.WriteRune(r)
			}
		}
	}
	if arg.Len() > 0 {
		args = append(args, arg.String())
	}
	return args
}

// ExtractOutput finds the primary output file (-o) in the compilation command.
func ExtractOutput(cmd CompileCommand) (string, error) {
	args := cmd.GetArgs()
	for i := 0; i < len(args)-1; i++ {
		if args[i] == "-o" {
			out := args[i+1]
			if !filepath.IsAbs(out) {
				out = filepath.Join(cmd.Directory, out)
			}
			return out, nil
		}
	}
	return "", fmt.Errorf("no output file found in command")
}

// PopulateTargets identifies the GN build targets for the files in the context.
func (ctx *WorkspaceContext) PopulateTargets() error {
	if ctx.BuildDir == "" {
		return nil
	}

	compDbPath := filepath.Join(ctx.BuildDir, "compile_commands.json")
	if _, err := os.Stat(compDbPath); os.IsNotExist(err) {
		return nil // No compilation database, can't find targets.
	}

	data, err := os.ReadFile(compDbPath)
	if err != nil {
		return fmt.Errorf("failed to read compile_commands.json: %w", err)
	}

	var commands []CompileCommand
	if err := json.Unmarshal(data, &commands); err != nil {
		return fmt.Errorf("failed to unmarshal compile_commands.json: %w", err)
	}

	// Maps for exact match and heuristic fallback.
	fileToCmd := make(map[string][]CompileCommand)
	dirToCmd := make(map[string]CompileCommand)
	dirAndBaseToCmd := make(map[string][]CompileCommand)

	for _, cmd := range commands {
		absFile := cmd.File
		if !filepath.IsAbs(absFile) {
			absFile = filepath.Join(cmd.Directory, absFile)
		}

		relFile := ctx.toRel(absFile)
		fileToCmd[relFile] = append(fileToCmd[relFile], cmd)

		relDir := filepath.Dir(relFile)
		// For the general directory fallback, we just need one example command
		// to "borrow" flags or output structure from.
		if _, ok := dirToCmd[relDir]; !ok {
			dirToCmd[relDir] = cmd
		}

		base := strings.TrimSuffix(filepath.Base(relFile), filepath.Ext(relFile))
		key := relDir + ":" + base
		dirAndBaseToCmd[key] = append(dirAndBaseToCmd[key], cmd)
	}

	// ninjaToLabel caches the resolution of Ninja paths to GN labels.
	ninjaToLabel := make(map[string]string)

	for i, f := range ctx.Files {
		if f.Status != StatusFound || f.IsDirectory {
			continue
		}

		// Only attempt to find targets for C-family files.
		ext := filepath.Ext(f.AbsPath)
		switch ext {
		case ".cc", ".cpp", ".cxx", ".c", ".h", ".hh", ".hpp":
			// proceed
		default:
			continue
		}

		relFile := ctx.toRel(f.AbsPath)
		var candidateCmds []CompileCommand
		relDir := filepath.Dir(relFile)

		if cmds, ok := fileToCmd[relFile]; ok {
			candidateCmds = cmds
		} else {
			// Heuristic 1: look for a file with the same base name in the same directory.
			base := strings.TrimSuffix(filepath.Base(relFile), filepath.Ext(relFile))
			if cmds, ok := dirAndBaseToCmd[relDir+":"+base]; ok {
				candidateCmds = cmds
			}
		}

		if len(candidateCmds) == 0 {
			// Heuristic 2: walk up the tree and find a file with similar base name.
			base := strings.TrimSuffix(filepath.Base(relFile), filepath.Ext(relFile))
			curr := filepath.Dir(relDir)
			for {
				if cmds, found := dirAndBaseToCmd[curr+":"+base]; found {
					candidateCmds = cmds
					break
				}
				parent := filepath.Dir(curr)
				if parent == curr {
					break
				}
				curr = parent
			}
		}

		if len(candidateCmds) == 0 {
			// Heuristic 3: try neighbor in the same directory.
			if cmd, ok := dirToCmd[relDir]; ok {
				candidateCmds = []CompileCommand{cmd}
			}
		}

		if len(candidateCmds) == 0 {
			f.BuildTargets = []string{unknownTarget}
			ctx.Files[i] = f
			continue
		}

		// Resolve all candidate commands to unique labels.
		var labels []string
		for _, cmd := range candidateCmds {
			output, err := ExtractOutput(cmd)
			if err != nil {
				continue
			}

			// build/api/client expects paths relative to the build directory.
			relOutput, err := filepath.Rel(ctx.BuildDir, output)
			if err != nil {
				relOutput = output
			}

			label, ok := ninjaToLabel[relOutput]
			if !ok {
				label, err = resolveNinjaPath(ctx, relOutput)
				if err != nil || label == "" {
					label = unknownTarget
				}
				ninjaToLabel[relOutput] = label
			}
			if label != unknownTarget {
				labels = append(labels, label)
			}
		}

		if len(labels) == 0 {
			f.BuildTargets = []string{unknownTarget}
		} else {
			// Deduplicate and sort labels, then pick the first one alphabetically.
			sort.Strings(labels)
			unique := labels[:0]
			for _, l := range labels {
				if len(unique) == 0 || l != unique[len(unique)-1] {
					unique = append(unique, l)
				}
			}
			f.BuildTargets = []string{unique[0]}
		}
		ctx.Files[i] = f
	}

	if err := ctx.VerifyBuild(); err != nil {
		return fmt.Errorf("failed during build verification: %w", err)
	}

	return nil
}

// VerifyBuild handles the synchronization of the shadow build directory and executes the build.
func (ctx *WorkspaceContext) VerifyBuild() error {
	if ctx.BuildDir == "" {
		ctx.setAnalysisErrorOnFoundFiles("Build directory not specified and .fx-build-dir is missing.")
		return nil
	}

	ideAnalysisDir := filepath.Join(ctx.FuchsiaDir, "out", ".ide-analysis")
	argsGnPath := filepath.Join(ctx.BuildDir, "args.gn")
	destArgsGnPath := filepath.Join(ideAnalysisDir, "args.gn")

	// 1. Check for args.gn in primary build directory.
	if _, err := os.Stat(argsGnPath); os.IsNotExist(err) {
		ctx.setAnalysisErrorOnFoundFiles("args.gn missing in primary build directory: %s", ctx.BuildDir)
		return nil
	}

	// 2. Sync args.gn and run fx gen if needed.
	changed, err := syncFile(argsGnPath, destArgsGnPath)
	if err != nil {
		ctx.setAnalysisErrorOnFoundFiles("failed to sync args.gn: %v", err)
		return nil
	}

	if changed {
		if err := ctx.runFx(ideAnalysisDir, "gen"); err != nil {
			ctx.setAnalysisErrorOnFoundFiles("fx gen failed: %v", err)
			return nil
		}
	}

	// 3. Collect targets and execute build.
	targets := make([]string, 0)
	targetToFiles := make(map[string][]int)
	for i, f := range ctx.Files {
		if f.Status == StatusNotFound {
			ctx.Files[i].AnalysisResult = &AnalysisResult{Status: AnalysisStatusNotFound}
			continue
		}
		if len(f.BuildTargets) == 0 || f.BuildTargets[0] == unknownTarget {
			ctx.Files[i].AnalysisResult = &AnalysisResult{Status: AnalysisStatusUnknown}
			continue
		}
		for _, t := range f.BuildTargets {
			if _, ok := targetToFiles[t]; !ok {
				targets = append(targets, t)
			}
			targetToFiles[t] = append(targetToFiles[t], i)
		}
	}

	if len(targets) == 0 {
		return nil
	}

	// 4. Execute build and populate results.
	// Try to build all targets at once first for performance.
	// If that fails, fall back to building them one by one to identify specific failures.

	batchArgs := append([]string{"build"}, targets...)
	batchErr := ctx.runFx(ideAnalysisDir, batchArgs...)

	if batchErr == nil {
		// All targets built successfully.
		for _, target := range targets {
			for _, i := range targetToFiles[target] {
				if ctx.Files[i].AnalysisResult == nil || ctx.Files[i].AnalysisResult.Status != AnalysisStatusBuildFailed {
					ctx.Files[i].AnalysisResult = &AnalysisResult{Status: AnalysisStatusOk}
				}
			}
		}
	} else {
		// Fallback to serial builds to identify specific failures.
		for _, target := range targets {
			buildErr := ctx.runFx(ideAnalysisDir, "build", target)
			for _, i := range targetToFiles[target] {
				if buildErr != nil {
					ctx.Files[i].AnalysisResult = &AnalysisResult{
						Status:  AnalysisStatusBuildFailed,
						Message: "File failed to build.",
					}
				} else {
					// Only set to OK if it hasn't been set as failed by another target.
					if ctx.Files[i].AnalysisResult == nil || ctx.Files[i].AnalysisResult.Status != AnalysisStatusBuildFailed {
						ctx.Files[i].AnalysisResult = &AnalysisResult{Status: AnalysisStatusOk}
					}
				}
			}
		}
	}

	return nil
}

// setAnalysisErrorOnFoundFiles sets the AnalysisError field on all files with StatusFound.
func (ctx *WorkspaceContext) setAnalysisErrorOnFoundFiles(format string, a ...interface{}) {
	errMsg := fmt.Sprintf(format, a...)
	for i := range ctx.Files {
		if ctx.Files[i].Status == StatusFound {
			ctx.Files[i].AnalysisError = errMsg
		}
	}
}

func (ctx *WorkspaceContext) runFx(dir string, args ...string) error {
	return runFx(ctx, dir, args...)
}

var runFx = func(ctx *WorkspaceContext, dir string, args ...string) error {
	fxPath := filepath.Join(ctx.FuchsiaDir, ".jiri_root", "bin", "fx")
	fullArgs := append([]string{"--dir", dir}, args...)
	cmd := exec.Command(fxPath, fullArgs...)
	cmd.Dir = ctx.FuchsiaDir
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("fx %v failed: %w\nOutput: %s", args, err, string(out))
	}
	return nil
}

func syncFile(src, dst string) (bool, error) {
	srcContent, err := os.ReadFile(src)
	if err != nil {
		return false, err
	}

	dstContent, err := os.ReadFile(dst)
	if err == nil && bytes.Equal(srcContent, dstContent) {
		return false, nil
	}

	if err := os.MkdirAll(filepath.Dir(dst), 0755); err != nil {
		return false, err
	}

	if err := os.WriteFile(dst, srcContent, 0644); err != nil {
		return false, err
	}

	return true, nil
}

// toRel normalizes a path to be relative to the Fuchsia root,
// handling cases where absolute paths might use different Cog/CartFS mount points.
func (ctx *WorkspaceContext) toRel(path string) string {
	// Try standard relative path first.
	rel, err := filepath.Rel(ctx.FuchsiaDir, path)
	if err == nil && !strings.HasPrefix(rel, "..") {
		return rel
	}

	// Heuristic: If we can't get a direct relative path (likely due to mount point mismatch),
	// find the part of the path that follows the fuchsia root basename.
	rootBase := filepath.Base(ctx.FuchsiaDir)
	search := "/" + rootBase + "/"
	if idx := strings.LastIndex(path, search); idx != -1 {
		return path[idx+len(search):]
	}

	return path
}

// resolveNinjaPath resolves a Ninja path to a GN label.
// This is a variable so it can be overridden in tests.
var resolveNinjaPath = func(ctx *WorkspaceContext, ninjaPath string) (string, error) {
	clientPath := filepath.Join(ctx.FuchsiaDir, "build", "api", "client")
	// Use --allow-unknown so we get the path back instead of an error if it's not found.
	args := []string{"ninja_path_to_gn_label", "--allow-unknown", ninjaPath}

	cmd := exec.Command(clientPath, args...)
	cmd.Dir = ctx.FuchsiaDir
	out, err := cmd.Output()
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			return "", fmt.Errorf("build/api/client failed with stderr: %s", string(exitErr.Stderr))
		}
		return "", err
	}

	label := strings.TrimSpace(string(out))
	if label == ninjaPath {
		return "", nil // Tool returned the input path, which means it's unknown.
	}
	return label, nil
}
