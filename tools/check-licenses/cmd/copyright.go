// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

type CopyrightCommand struct {
	fuchsiaDir  string
	printStdout bool
}

func (*CopyrightCommand) Name() string     { return "copyright" }
func (*CopyrightCommand) Synopsis() string { return "Check or add copyright headers to files." }
func (*CopyrightCommand) Usage() string {
	return `copyright [-stdout] <file_path>:
  Analyzes a file to determine if it contains the Fuchsia copyright header.
  If the header is missing, it will be prepended and the file overwritten in-place.
  Use -stdout to print the newly formatted text to stdout without modifying the file.
`
}

func (c *CopyrightCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
	f.BoolVar(&c.printStdout, "stdout", false, "Print the formatted text to stdout instead of overwriting the file.")
}

func (c *CopyrightCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	absPath, err := filepath.Abs(targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to resolve absolute path for %s: %v\n", targetFile, err)
		return subcommands.ExitFailure
	}

	if c.fuchsiaDir == "" {
		c.fuchsiaDir = "."
	}

	// Find the patterns directory so the classifier knows what "FuchsiaCopyright" looks like
	patternsDir := filepath.Join(c.fuchsiaDir, "tools", "check-licenses", "assets", "patterns")

	// Instantiate the v2 stateless Classifier
	classifier, err := classify.NewClassifier(0.8, []string{patternsDir}, nil)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	// Set up the pipeline channels for a single file
	inChan := make(chan pipeline.FilteredProject, 1)
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: filepath.Dir(absPath),
			Files:    []pipeline.FileInfo{{Path: absPath}},
		},
	}
	close(inChan)

	outChan, err := classifier.Run(ctx, inChan)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to run classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	// Consume the result
	var result pipeline.ClassifiedFile
	for cf := range outChan {
		result = cf
	}

	hasCopyright := false
	for _, match := range result.Matches {
		if match.SPDXID == "FuchsiaCopyright" {
			hasCopyright = true
			break
		}
	}

	if hasCopyright {
		if c.printStdout {
			// Even if it has a copyright, we must print the file content to stdout
			// so that a SHAC formatter check sees no changes.
			content, _ := os.ReadFile(absPath)
			fmt.Print(string(content))
		} else {
			fmt.Fprintf(os.Stderr, "✅ Fuchsia copyright header found in %s\n", targetFile)
		}
		return subcommands.ExitSuccess
	}

	if !c.printStdout {
		fmt.Fprintf(os.Stderr, "❌ Fuchsia copyright header missing in %s\n", targetFile)
	}

	// Remediation logic: Construct the new file with the copyright header
	newBytes, err := c.addCopyright(absPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to add copyright: %v\n", err)
		return subcommands.ExitFailure
	}

	if c.printStdout {
		// Print to stdout (required for SHAC formatters)
		fmt.Print(string(newBytes))
		return subcommands.ExitSuccess
	}

	if err := os.WriteFile(absPath, newBytes, 0644); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to write file: %v\n", err)
		return subcommands.ExitFailure
	}
	fmt.Fprintf(os.Stderr, "✏️  Successfully added Fuchsia copyright header to %s\n", targetFile)

	return subcommands.ExitSuccess
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

func (c *CopyrightCommand) addCopyright(filePath string) ([]byte, error) {
	ext := filepath.Ext(filePath)

	commentPrefix, ok := commentPrefixes[ext]
	if !ok {
		// Do not guess. If we guess wrong, we corrupt the build.
		return nil, fmt.Errorf("unsupported file extension %q for automatic copyright injection", ext)
	}

	year := time.Now().Year()

	var header string
	if commentPrefix == "rem" {
		header = fmt.Sprintf("%s Copyright %d The Fuchsia Authors. All rights reserved.\n%s Use of this source code is governed by a BSD-style license that can be\n%s found in the LICENSE file.\n\n", commentPrefix, year, commentPrefix, commentPrefix)
	} else {
		header = fmt.Sprintf("%s Copyright %d The Fuchsia Authors. All rights reserved.\n%s Use of this source code is governed by a BSD-style license that can be\n%s found in the LICENSE file.\n\n", commentPrefix, year, commentPrefix, commentPrefix)
	}

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
