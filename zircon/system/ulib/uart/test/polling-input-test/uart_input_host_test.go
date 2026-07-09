// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package polling_input_test

import (
	"context"
	"flag"
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/emulator"
	"go.fuchsia.dev/fuchsia/tools/emulator/emulatortest"
)

var zbiPathFlag = flag.String("zbi-path", "", "Path to the custom ZBI")

const (
	// LINT.IfChange
	// Command passed through serial, \n or \r is appended at the end.
	serialCommand string = "RandomString1234!"

	// Prefix added by physboot, determined by the program name.
	serialPrefix string = "uart-input-test: "

	// String printed when the test is listening for characters in serial.
	serialInputReady string = serialPrefix + "UartInputReady"

	// String printed when the test received all characters and prints it back.
	serialDone string = serialPrefix + "Received " + serialCommand
	// LINT.ThenChange(./uart-input-test.cc)

	zbiName string = "uart-input-test-zbi"
)

func getCwd(t *testing.T) string {
	ex, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	return filepath.Dir(ex)
}

func TestUartInputIsParsedCorrectly(t *testing.T) {
	cwd := getCwd(t)
	distro := emulatortest.UnpackFrom(t, filepath.Join(cwd, "test_data"), emulator.DistributionParams{Emulator: emulator.Qemu})
	if *zbiPathFlag == "" {
		t.Fatal("-zbi-path flag is required")
	}
	absZbiPath, err := filepath.Abs(*zbiPathFlag)
	if err != nil {
		t.Fatal(err)
	}
	distro.OverrideImage(zbiName, "zbi", absZbiPath)
	arch := distro.TargetCPU()
	device := emulator.DefaultVirtualDevice(string(arch))
	device.Initrd = zbiName

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	i := distro.NewInstance(ctx, device)
	i.Start()

	i.WaitForLogMessages([]string{serialInputReady})
	i.RunCommand(serialCommand)
	i.WaitForLogMessages([]string{serialDone})
}
