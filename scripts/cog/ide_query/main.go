// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/pflag"
)

type FileStatus string

const (
	StatusFound    FileStatus = "found"
	StatusNotFound FileStatus = "not_found"
)

type FileEntry struct {
	// AbsPath is the absolute, canonicalized path to the file.
	AbsPath string `json:"abs_path"`
	// OriginalPath is the path as provided by the user (relative or absolute).
	OriginalPath string `json:"original_path"`
	// Status indicates if the file exists on disk.
	Status FileStatus `json:"status"`
	// IsDirectory is true if the path points to a directory.
	IsDirectory bool `json:"is_directory"`
	// BuildTargets is a list of GN labels that need to be built for this file.
	BuildTargets []string `json:"build_targets,omitempty"`
}

type WorkspaceContext struct {
	// FuchsiaDir is the absolute path to the root of the Fuchsia checkout.
	FuchsiaDir string `json:"fuchsia_dir"`
	// BuildDir is the absolute path to the current build output directory.
	// This may be empty if not provided and .fx-build-dir is missing.
	BuildDir string `json:"build_dir"`
	// Files is a list of file entries provided via the --file flag or --file-list flag.
	Files []FileEntry `json:"files"`
}

type ErrorResponse struct {
	// Error is a human-readable error message.
	Error string `json:"error"`
}

func main() {
	ctx, err := NewWorkspaceContext(os.Args[1:])
	if err != nil {
		resp := ErrorResponse{Error: err.Error()}
		encoder := json.NewEncoder(os.Stdout)
		encoder.SetIndent("", "  ")
		if err := encoder.Encode(resp); err != nil {
			fmt.Fprintf(os.Stderr, "failed to encode error response: %v\n", err)
		}
		os.Exit(1)
	}

	encoder := json.NewEncoder(os.Stdout)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(ctx); err != nil {
		fmt.Fprintf(os.Stderr, "failed to encode workspace context: %v\n", err)
		os.Exit(1)
	}
}

func NewWorkspaceContext(args []string) (*WorkspaceContext, error) {
	fs := pflag.NewFlagSet("ide-query", pflag.ContinueOnError)

	var fuchsiaDir string
	var buildDir string
	var files []string
	var fileLists []string

	fs.StringVar(&fuchsiaDir, "fuchsia-dir", "", "Overrides the Fuchsia root directory.")
	fs.StringVar(&buildDir, "build-dir", "", "Overrides the build output directory.")
	fs.StringSliceVar(&files, "file", nil, "A path to a file to be queried. Can be repeated.")
	fs.StringSliceVar(&fileLists, "file-list", nil, "A path to a file containing a list of files to be queried. Can be repeated.")

	if err := fs.Parse(args); err != nil {
		return nil, err
	}

	if fuchsiaDir == "" {
		return nil, fmt.Errorf("--fuchsia-dir is required")
	}

	// Canonicalize FuchsiaDir
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err != nil {
		return nil, fmt.Errorf("invalid fuchsia-dir: %w", err)
	}
	fuchsiaDir, err = filepath.EvalSymlinks(absFuchsiaDir)
	if err != nil {
		return nil, fmt.Errorf("failed to resolve fuchsia-dir: %w", err)
	}
	if info, err := os.Stat(fuchsiaDir); err != nil || !info.IsDir() {
		return nil, fmt.Errorf("fuchsia-dir does not exist or is not a directory: %s", fuchsiaDir)
	}

	// Resolve BuildDir
	if buildDir == "" {
		buildDirFile := filepath.Join(fuchsiaDir, ".fx-build-dir")
		if content, err := os.ReadFile(buildDirFile); err == nil {
			line := strings.TrimSpace(string(content))
			if line != "" {
				if filepath.IsAbs(line) {
					return nil, fmt.Errorf(".fx-build-dir contains absolute path: %s", line)
				}
				buildDir = filepath.Join(fuchsiaDir, line)
			}
		}
	} else {
		if !filepath.IsAbs(buildDir) {
			absBuildDir, err := filepath.Abs(buildDir)
			if err != nil {
				return nil, fmt.Errorf("failed to get absolute path for build-dir: %w", err)
			}
			buildDir = absBuildDir
		}
	}

	if buildDir != "" {
		buildDir, err = filepath.EvalSymlinks(buildDir)
		if err != nil {
			return nil, fmt.Errorf("failed to resolve build-dir: %w", err)
		}
		if info, err := os.Stat(buildDir); err != nil || !info.IsDir() {
			return nil, fmt.Errorf("build-dir does not exist or is not a directory: %s", buildDir)
		}
	}

	// Collect file paths from flags and files
	rawPaths := make([]string, 0, len(files))
	for _, f := range files {
		if f == "" {
			return nil, fmt.Errorf("empty path provided via --file")
		}
		rawPaths = append(rawPaths, f)
	}

	for _, listFile := range fileLists {
		listPaths, err := readFileList(listFile)
		if err != nil {
			return nil, fmt.Errorf("failed to read file-list %s: %w", listFile, err)
		}
		rawPaths = append(rawPaths, listPaths...)
	}

	// Process and deduplicate file entries
	entries := make([]FileEntry, 0)
	seen := make(map[string]int) // maps canonical path to index in entries

	for _, p := range rawPaths {
		entry, err := resolveFileEntry(fuchsiaDir, p)
		if err != nil {
			return nil, err
		}

		if idx, ok := seen[entry.AbsPath]; ok {
			entries[idx] = entry
		} else {
			seen[entry.AbsPath] = len(entries)
			entries = append(entries, entry)
		}
	}

	ctx := &WorkspaceContext{
		FuchsiaDir: fuchsiaDir,
		BuildDir:   buildDir,
		Files:      entries,
	}

	if err := ctx.PopulateTargets(); err != nil {
		return nil, fmt.Errorf("failed to populate build targets: %w", err)
	}

	return ctx, nil
}

func readFileList(name string) ([]string, error) {
	f, err := os.Open(name)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var paths []string
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			return nil, fmt.Errorf("empty line in file-list %s", name)
		}
		if strings.HasPrefix(line, "#") {
			continue
		}
		paths = append(paths, line)
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	return paths, nil
}

func resolveFileEntry(fuchsiaDir, path string) (FileEntry, error) {
	originalPath := path
	absPath := path
	if !filepath.IsAbs(path) {
		absPath = filepath.Join(fuchsiaDir, path)
	}

	canonicalPath, status, isDir, err := canonicalize(absPath)
	if err != nil {
		return FileEntry{}, err
	}

	return FileEntry{
		AbsPath:      canonicalPath,
		OriginalPath: originalPath,
		Status:       status,
		IsDirectory:  isDir,
	}, nil
}

func canonicalize(path string) (string, FileStatus, bool, error) {
	// filepath.EvalSymlinks only works for existing paths.
	// For missing paths, we need to resolve symlinks for parent directories.

	info, err := os.Stat(path)
	if err == nil {
		canonical, err := filepath.EvalSymlinks(path)
		if err != nil {
			return "", "", false, err
		}
		return canonical, StatusFound, info.IsDir(), nil
	}

	if os.IsNotExist(err) {
		// Resolve parent symlinks
		dir := filepath.Dir(path)
		canonicalDir, err := filepath.EvalSymlinks(dir)
		if err != nil {
			// If parent doesn't exist either, we just take the path as-is but canonicalized.
			// However, EvalSymlinks usually fails if path doesn't exist.
			// Let's try to resolve the closest existing parent.
			// Wait, the design doc says: "resolve symlinks for all existing parent directories and then append the missing filename to the canonicalized parent path."

			// Simple approach: recurse to find existing parent.
			for {
				canonicalDir, err = filepath.EvalSymlinks(dir)
				if err == nil {
					break
				}
				if dir == "/" || dir == "." || dir == filepath.Dir(dir) {
					// Root reached or no progress.
					return path, StatusNotFound, false, nil
				}
				dir = filepath.Dir(dir)
			}
			// Reconstruct relative to canonicalDir
			rel, _ := filepath.Rel(dir, path)
			return filepath.Join(canonicalDir, rel), StatusNotFound, false, nil
		}
		return filepath.Join(canonicalDir, filepath.Base(path)), StatusNotFound, false, nil
	}

	return "", "", false, err
}
