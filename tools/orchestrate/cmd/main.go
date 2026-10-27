// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"os"
	"os/signal"
	"syscall"

	"github.com/google/subcommands"
)

func main() {
	subcommands.Register(&runCmd{}, "")
	subcommands.Register(subcommands.HelpCommand(), "")

	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	exitStatus := subcommands.Execute(ctx)
	stop()
	os.Exit(int(exitStatus))
}
