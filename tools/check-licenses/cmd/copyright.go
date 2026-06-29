// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/google/subcommands"
)

type CopyrightCommand struct {
	fuchsiaDir string
}

func (*CopyrightCommand) Name() string     { return "copyright" }
func (*CopyrightCommand) Synopsis() string { return "Check or add copyright headers to files." }
func (*CopyrightCommand) Usage() string {
	return `copyright <subcommand> [options]:
  Manage copyright headers.

  Subcommands:
    add   Add copyright header to file if missing.
    check Check if file has copyright header.
`
}

func (c *CopyrightCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
}

func (c *CopyrightCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("copyright", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "copyright")
	subCommander.Register(&CopyrightAddCommand{fuchsiaDir: c.fuchsiaDir}, "")
	subCommander.Register(&CopyrightCheckCommand{fuchsiaDir: c.fuchsiaDir}, "")
	return subCommander.Execute(ctx)
}

type CopyrightAddCommand struct {
	fuchsiaDir  string
	printStdout bool
}

func (*CopyrightAddCommand) Name() string     { return "add" }
func (*CopyrightAddCommand) Synopsis() string { return "Add copyright header to file if missing." }
func (*CopyrightAddCommand) Usage() string {
	return `add [-stdout] <file_path>:
  Adds a Fuchsia copyright header to the file if missing.
  Use -stdout to print result to stdout instead of modifying file.
`
}

func (c *CopyrightAddCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the formatted text to stdout instead of overwriting the file.")
}

func (c *CopyrightAddCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	fuchsiaDir, targetFile, err := ResolveAndValidatePath(c.fuchsiaDir, targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	if err := ApplyCopyrightFix(fuchsiaDir, targetFile, c.printStdout); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}

type CopyrightCheckCommand struct {
	fuchsiaDir string
	fileList   string
}

func (*CopyrightCheckCommand) Name() string     { return "check" }
func (*CopyrightCheckCommand) Synopsis() string { return "Check if file has copyright header." }
func (*CopyrightCheckCommand) Usage() string {
	return `check [-file-list <path>] [<file_path>...]:
  Checks if the files contain the Fuchsia copyright header.
  Use -file-list to specify a file containing paths to check, one per line.
  Prints the paths of files missing headers to stdout.
  Fails with exit code 1 if any file is missing a header.
`
}

func (c *CopyrightCheckCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fileList, "file-list", "", "Path to a file containing a list of file paths to check, one per line.")
}

func (c *CopyrightCheckCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	var targets []string

	if c.fileList != "" {
		absFileList := c.fileList
		if !filepath.IsAbs(absFileList) {
			absFileList = filepath.Join(c.fuchsiaDir, c.fileList)
		}
		content, err := os.ReadFile(absFileList)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error reading file list %s: %v\n", c.fileList, err)
			return subcommands.ExitStatus(3) // System error
		}
		for _, line := range strings.Split(string(content), "\n") {
			line = strings.TrimSpace(line)
			if line != "" {
				targets = append(targets, line)
			}
		}
	} else {
		targets = f.Args()
	}

	if len(targets) == 0 {
		fmt.Fprintln(os.Stderr, "Error: at least one file path must be provided, or use -file-list.")
		return subcommands.ExitUsageError
	}

	hasErrors := false
	hasMissingCopyright := false
	for _, targetFile := range targets {
		fuchsiaDir, resolvedPath, err := ResolveAndValidatePath(c.fuchsiaDir, targetFile)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error resolving %s: %v\n", targetFile, err)
			hasErrors = true
			continue
		}

		hasCopyright, err := CheckCopyright(fuchsiaDir, resolvedPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error checking %s: %v\n", targetFile, err)
			hasErrors = true
			continue
		}

		if !hasCopyright {
			// Print failed file path to stdout (one per line) for easy parsing
			fmt.Println(targetFile)
			hasMissingCopyright = true
		}
	}

	if hasErrors {
		return subcommands.ExitStatus(3) // System/IO error
	}
	if hasMissingCopyright {
		return subcommands.ExitFailure // 1
	}
	return subcommands.ExitSuccess // 0
}

var commentCleaner = strings.NewReplacer(
	"//", " ",
	"/*", " ",
	"*/", " ",
	"#", " ",
	";", " ",
	"*", " ",
	"\r", " ",
	"\n", " ",
	"\t", " ",
)

// Standard Fuchsia/Chromium/Android copyright regex (strict).
// It matches the exact core license text to enforce the correct standard.
// It ignores comment prefixes and other whitespace text via commentCleaner.
var copyrightRegex = regexp.MustCompile(
	`(?i)Copyright\s+[0-9,\-\s]+The\s+Fuchsia\s+Authors\.?\s*` +
		`(?:\s*All\s+rights\s+reserved\.?)?\s+` +
		`Use\s+of\s+this\s+source\s+code\s+is\s+governed\s+by\s+a\s+BSD-style\s+license\s+` +
		`that\s+can\s+be\s+found\s+in\s+the\s+LICENSE\s+file`,
)

// CheckCopyright verifies if a file has a Fuchsia copyright header.
func CheckCopyright(fuchsiaDir, filePath string) (bool, error) {
	absPath := filePath
	if !filepath.IsAbs(filePath) {
		absPath = filepath.Join(fuchsiaDir, filePath)
	}

	// Skip empty files (size 0). They are not required to have copyright headers.
	stat, err := os.Stat(absPath)
	if err == nil && stat.Size() == 0 {
		return true, nil
	}

	// We only need to read the beginning of the file.
	// 8192 bytes should be more than enough for the copyright header,
	// even if there are large third-party headers before it.
	f, err := os.Open(absPath)
	if err != nil {
		return false, err
	}
	defer f.Close()

	buf := make([]byte, 8192)
	n, err := f.Read(buf)
	if err != nil && err != io.EOF {
		return false, err
	}

	cleaned := commentCleaner.Replace(string(buf[:n]))
	return copyrightRegex.MatchString(cleaned), nil
}

// ApplyCopyrightFix analyzes a file and adds a Fuchsia copyright header if missing.
// Note: If a file has a nearly-correct but non-matching copyright header, this
// tool will prepend a new one, resulting in double headers. The developer must
// manually resolve this.
func ApplyCopyrightFix(fuchsiaDir, filePath string, printStdout bool) error {
	absPath := filePath
	if !filepath.IsAbs(filePath) {
		absPath = filepath.Join(fuchsiaDir, filePath)
	}

	hasCopyright, err := CheckCopyright(fuchsiaDir, absPath)
	if err != nil {
		return err
	}

	if hasCopyright {
		if printStdout {
			content, err := os.ReadFile(absPath)
			if err != nil {
				return fmt.Errorf("failed to read file %s: %w", absPath, err)
			}
			fmt.Print(string(content))
		} else {
			fmt.Printf("✅ Fuchsia copyright header found in %s\n", filePath)
		}
		return nil
	}

	if !printStdout {
		fmt.Printf("❌ Fuchsia copyright header missing in %s\n", filePath)
	}

	newBytes, err := addCopyright(absPath)
	if err != nil {
		return err
	}

	if printStdout {
		fmt.Print(string(newBytes))
		return nil
	}

	if err := os.WriteFile(absPath, newBytes, 0644); err != nil {
		return fmt.Errorf("failed to write file: %w", err)
	}
	fmt.Printf("✏️  Successfully added Fuchsia copyright header to %s\n", filePath)

	return nil
}

var commentPrefixes = map[string]string{
	// C-style comments
	".c": "//", ".cc": "//", ".cpp": "//", ".h": "//", ".hh": "//", ".hpp": "//",
	".inc": "//", ".go": "//", ".rs": "//", ".dart": "//", ".java": "//", ".js": "//",
	".m": "//", ".cml": "//", ".fidl": "//", ".d": "//", ".dat": "//",
	// Script/Config-style comments
	".py": "#", ".sh": "#", ".gn": "#", ".gni": "#", ".gyp": "#", ".gypi": "#",
	".merkle": "#", ".ac": "#", ".am": "#",
	// Assembly
	".asm": ";",
	// Windows Batch
	".bat": "rem",
}

func addCopyright(filePath string) ([]byte, error) {
	ext := filepath.Ext(filePath)

	commentPrefix, ok := commentPrefixes[ext]
	if !ok {
		// Do not guess. If we guess wrong, we corrupt the build.
		return nil, fmt.Errorf("unsupported file extension %q for automatic copyright injection", ext)
	}

	year := time.Now().Year()

	header := fmt.Sprintf("%s Copyright %d The Fuchsia Authors. All rights reserved.\n%s Use of this source code is governed by a BSD-style license that can be\n%s found in the LICENSE file.\n\n", commentPrefix, year, commentPrefix, commentPrefix)

	content, err := os.ReadFile(filePath)
	if err != nil {
		return nil, err
	}

	// Prepend the header
	var newContent bytes.Buffer

	// Handle shebangs (e.g. #!/bin/bash)
	if bytes.HasPrefix(content, []byte("#!")) {
		lines := bytes.SplitN(content, []byte("\n"), 2)
		newContent.Write(lines[0])
		newContent.WriteString("\n\n")
		newContent.WriteString(header)
		if len(lines) > 1 {
			newContent.Write(lines[1])
		}
	} else {
		newContent.WriteString(header)
		newContent.Write(content)
	}

	return newContent.Bytes(), nil
}
