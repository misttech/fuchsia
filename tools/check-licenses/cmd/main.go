// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"os"
	"strings"

	"github.com/google/subcommands"
)

func main() {
	// Initialize the subcommand router
	commander := subcommands.NewCommander(flag.CommandLine, "check-licenses")

	// Register subcommands package's built-in commands
	commander.Register(commander.HelpCommand(), "")
	commander.Register(commander.FlagsCommand(), "")
	commander.Register(commander.CommandsCommand(), "")

	// Register our tool's commands
	commander.Register(&GenerateCommand{}, "")
	commander.Register(&ValidateCommand{}, "")
	commander.Register(&FixCommand{}, "")
	commander.Register(&ReadmeCommand{}, "")
	commander.Register(&ProjectCommand{}, "")
	commander.Register(&PolicyCommand{}, "")
	commander.Register(&AllowlistCommand{}, "")
	commander.Register(&CopyrightCommand{}, "")
	commander.Register(&ClassifyCommand{}, "")

	// Fallback routing for backward compatibility
	// If no valid subcommand is provided, we insert "generate" into os.Args.
	knownCommands := map[string]bool{
		"generate":  true,
		"validate":  true,
		"fix":       true,
		"readme":    true,
		"project":   true,
		"policy":    true,
		"allowlist": true,
		"copyright": true,
		"classify":  true,
		"help":      true,
		"flags":     true,
		"commands":  true,
	}

	insertGenerate := true
	for _, arg := range os.Args[1:] {
		if !strings.HasPrefix(arg, "-") {
			if knownCommands[arg] {
				insertGenerate = false
			}
			break
		}
	}

	if insertGenerate {
		os.Args = append([]string{os.Args[0], "generate"}, os.Args[1:]...)
	}

	flag.Parse()
	ctx := context.Background()

	// Dispatch execution
	os.Exit(int(commander.Execute(ctx)))
}
