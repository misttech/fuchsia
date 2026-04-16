// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"

	"github.com/google/subcommands"
	"go.fuchsia.dev/fuchsia/tools/sdk-tools/fssh/fssh"
)

const (
	fsshCmdName = "fssh"
)

func insertFSSHToCmd(args []string, position int) []string {
	if len(args) == position {
		return append(args, fsshCmdName)
	}
	args = append(args[:position+1], args[position:]...)
	args[position] = fsshCmdName
	return args
}

func main() {
	fmt.Fprintf(os.Stderr, "[Warning] fssh is being deprecated! See go/fssh-manual for its replacement manual steps.\n")
	fmt.Fprintf(os.Stderr, "Questions? Reach out in go/smart-display-developers-chat.\n\n")

	if isTerminal() {
		fmt.Print("Press Enter to acknowledge and continue...")
		fmt.Scanln()
		fmt.Println("")
	}

	// Hack to support a default subcommand.
	if len(os.Args) == 1 || (os.Args[1] != "tunnel" && os.Args[1] != "sync-keys" && os.Args[1] != "fssh") {
		os.Args = insertFSSHToCmd(os.Args, 1)
	}

	subcommands.Register(subcommands.HelpCommand(), "")
	subcommands.Register(subcommands.FlagsCommand(), "")
	subcommands.Register(subcommands.CommandsCommand(), "")
	subcommands.Register(&fssh.Cmd{}, "")
	subcommands.Register(&fssh.TunnelCmd{}, "")
	subcommands.Register(&fssh.SyncKeysCmd{}, "")

	flag.Parse()
	ctx := context.Background()
	os.Exit(int(subcommands.Execute(ctx)))
}

func isTerminal() bool {
	fileInfo, err := os.Stdin.Stat()
	if err != nil {
		return false
	}
	return (fileInfo.Mode() & os.ModeCharDevice) != 0
}
