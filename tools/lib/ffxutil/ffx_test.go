// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffxutil

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/clock"
)

func TestFFXInstance(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), os.ModePerm); err != nil {
		t.Fatal("failed to write mock ffx tool")
	}
	fakeClock := clock.NewFakeClock()
	ctx := clock.NewContext(context.Background(), fakeClock)
	sshPriv := filepath.Join(tmpDir, "privKey")
	sshPub := filepath.Join(tmpDir, "pubKey")
	sshKeys := SSHInfo{SshPriv: sshPriv, SshPub: sshPub}
	ffx, _ := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "target", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXLegacy)

	var buf []byte
	stdout := bytes.NewBuffer(buf)
	ffx.SetStdoutStderr(stdout, stdout)

	assertRunsExpectedCmd := func(runErr error, stdout *bytes.Buffer, expectedCmd string) {
		if runErr != nil {
			t.Errorf("failed to run cmd: %s", runErr)
		}
		stdoutStr := stdout.String()
		if !strings.HasSuffix(strings.TrimSpace(stdoutStr), expectedCmd) {
			t.Errorf("got %q, want %q", stdoutStr, expectedCmd)
		}
	}
	assertRunsExpectedCmd(ffx.List(ctx), stdout, "target list")

	assertRunsExpectedCmd(ffx.TargetWait(ctx), stdout, "--target target --config log.level=debug target wait")

	assertRunsExpectedCmd(ffx.TargetWait(ctx, "-t", "90"), stdout, "--target target --config log.level=debug target wait -t 90")

	// Create a new instance that uses the same ffx config but runs against a different target.
	ffx2 := FFXWithTarget(ffx, "target2")
	var buf2 []byte
	stdout2 := bytes.NewBuffer(buf2)
	ffx2.SetStdoutStderr(stdout2, stdout2)
	assertRunsExpectedCmd(ffx2.TargetWait(ctx), stdout2, "--target target2 --config log.level=debug target wait")

	// Test expects a run_summary.json to be written in the test output directory.
	outDir := filepath.Join(tmpDir, "out")
	testOutputDir := filepath.Join(outDir, "test-outputs")
	if err := os.MkdirAll(testOutputDir, os.ModePerm); err != nil {
		t.Errorf("failed to create test outputs dir: %s", err)
	}
	runSummaryBytes := []byte("{\"schema_id\": \"https://fuchsia.dev/schema/ffx_test/run_summary-8d1dd964.json\"}")
	if err := os.WriteFile(filepath.Join(testOutputDir, runSummaryFilename), runSummaryBytes, os.ModePerm); err != nil {
		t.Errorf("failed to write run_summary.json: %s", err)
	}
	_, err := ffx.TestRun(ctx, build.TestList{}, outDir)
	assertRunsExpectedCmd(
		err,
		stdout,
		fmt.Sprintf(
			"--target target test run --continue-on-timeout --test-file %s --output-directory %s --show-full-moniker-in-logs",
			filepath.Join(outDir, "test-list.json"), testOutputDir,
		),
	)

	// Snapshot expects a file to be written to tmpDir/snapshotZipName which it will move to tmpDir/new_snapshot.zip.
	if err := os.WriteFile(filepath.Join(tmpDir, snapshotZipName), []byte("snapshot"), os.ModePerm); err != nil {
		t.Errorf("failed to write snapshot")
	}
	assertRunsExpectedCmd(ffx.Snapshot(ctx, tmpDir, "new_snapshot.zip"), stdout, "--target target target snapshot --dir "+tmpDir)
	if _, err := os.Stat(filepath.Join(tmpDir, snapshotZipName)); err == nil {
		t.Errorf("expected snapshot to be renamed")
	}
	if _, err := os.Stat(filepath.Join(tmpDir, "new_snapshot.zip")); err != nil {
		t.Errorf("failed to rename snapshot to new_snapshot.zip: %s", err)
	}

	assertRunsExpectedCmd(ffx.GetConfig(ctx), stdout, "config get")

	assertRunsExpectedCmd(ffx.Run(ctx, "random", "cmd", "with", "args"), stdout, "random cmd with args")

	assertRunsExpectedCmd(ffx.RunWithTarget(ctx, "random", "cmd", "with", "args"), stdout, "--target target random cmd with args")

	assertRunsExpectedCmd(ffx.StartFFXMonitor(ctx, "8080", "log.json", "aggregations.json"), stdout, "monitor start --nodename target --port 8080 --no-usb --log-file log.json --aggregations-file aggregations.json")

	assertRunsExpectedCmd(ffx.StartFFXMonitor(ctx, "8081", "", ""), stdout, "monitor start --nodename target --port 8081 --no-usb")

	if err := ffx.Stop(); err != nil {
		t.Errorf("ffx.Stop() = %s", err)
	}

	fPrivKey := ffx.GetSshPrivateKey()
	if sshPriv != fPrivKey {
		t.Errorf("got wrong private key: %s (expected %s)", fPrivKey, sshPriv)
	}
	fPubKey := ffx.GetSshAuthorizedKeys()
	if sshPub != fPubKey {
		t.Errorf("got wrong private key: %s (expected %s)", fPubKey, sshPub)
	}
}

func TestFFXPBArtifacts(t *testing.T) {
	for _, testcase := range []struct {
		name      string
		output    string
		errOutput string
		exitCode  int
		wantPaths []string
		wantError error
	}{
		{
			name:      "OK paths",
			output:    `{"ok": {"paths": [ "pb1.txt", "pb2.txt"]}}`,
			errOutput: "",
			exitCode:  0,
			wantPaths: []string{"pb1.txt", "pb2.txt"},
			wantError: nil,
		},
		{
			name:      "pb not found paths",
			output:    `{"user_error": {"message": "path not found"}}`,
			errOutput: "path not found",
			exitCode:  1,
			wantPaths: []string{},
			wantError: fmt.Errorf("user error: %s", "path not found"),
		},
		{
			name:      "pb not found paths",
			output:    `{"unexpected_error": {"message": "somthing went wrong"}}`,
			errOutput: "exception processing metadata",
			exitCode:  1,
			wantPaths: []string{},
			wantError: fmt.Errorf("unexpected error: somthing went wrong"),
		},
	} {
		t.Run(testcase.name, func(t *testing.T) {
			var testErr error = nil
			if testcase.exitCode != 0 {
				testErr = fmt.Errorf("Exit Code %d: %s", testcase.exitCode, testcase.errOutput)
			}
			paths, err := processPBArtifactsResult(testcase.output, testErr)

			if err != nil {
				if testcase.wantError != nil {
					if err.Error() != testcase.wantError.Error() {
						t.Errorf("Got error %q wanted error: %q", err, testcase.wantError)
					}
				} else if err != nil {
					t.Errorf("Test error: %s", err)
				}
			}

			if len(paths) != len(testcase.wantPaths) {
				t.Errorf("Length mismatch Got  %v want %v", paths, testcase.wantPaths)
			}
			for i := range paths {
				if paths[i] != testcase.wantPaths[i] {
					t.Errorf("mismatch index %d. Got  %v want %v", i, paths, testcase.wantPaths)
				}
			}
		})
	}
}

func TestEmuStartConsoleDryRunError(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := "#!/bin/bash\nif [[ \"$*\" == *\"--dry-run\"* ]]; then\n  echo '{\"user_error\":{\"message\":\"There are no virtual devices configured for this product bundle\"}}' >&2\n  exit 1\nfi\necho \"$@\"\n"
	if err := os.WriteFile(ffxPath, []byte(script), os.ModePerm); err != nil {
		t.Fatalf("failed to write mock ffx tool: %s", err)
	}
	ctx := context.Background()
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "target", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXLegacy)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}
	cmd, err := ffx.EmuStartConsole(ctx, tmpDir, "fuchsia-emulator", EmuTools{}, EmuStartArgs{
		Engine:        "qemu",
		ProductBundle: filepath.Join(tmpDir, "pb"),
	})
	if err == nil {
		t.Errorf("expected EmuStartConsole to fail when --dry-run fails, got nil error and cmd %v", cmd)
	}
}

func TestLegacyFfxCmdBuilderDeterministicConfigs(t *testing.T) {
	builder := newLegacyFfxCmdBuilder("/tmp/isolate", "/tmp/config")
	got := builder.commandWithConfigs("/bin/ffx", []string{"target", "show"}, map[string]any{
		"z.key": "zval",
		"a.key": "aval",
		"m.key": "mval",
	})
	want := []string{
		"/bin/ffx", "--isolate-dir", "/tmp/isolate",
		"-c", "a.key=aval",
		"-c", "m.key=mval",
		"-c", "z.key=zval",
		"target", "show",
	}
	if !slices.Equal(got, want) {
		t.Errorf("commandWithConfigs() = %v, want %v", got, want)
	}
}

func TestStrictFfxCmdBuilderDeterministicConfigs(t *testing.T) {
	builder := newStrictFfxCmdBuilder("/tmp/ffx.log")
	if err := builder.setConfigMap(
		map[string]any{"user.key": "uval", "shared.key": "from_user"},
		map[string]any{"z.key": "zval", "a.key": "aval", "shared.key": "from_global"},
	); err != nil {
		t.Fatalf("setConfigMap failed: %s", err)
	}
	got := builder.commandWithConfigs("/bin/ffx", []string{"target", "show"}, map[string]any{
		"m.key":      "mval",
		"shared.key": "from_call",
	})
	want := []string{
		"/bin/ffx", "-o", "/tmp/ffx.log", "--strict", "--machine", "json",
		"-c", "a.key=aval",
		"-c", "m.key=mval",
		"-c", "shared.key=from_call",
		"-c", "user.key=uval",
		"-c", "z.key=zval",
		"target", "show",
	}
	if !slices.Equal(got, want) {
		t.Errorf("commandWithConfigs() = %v, want %v", got, want)
	}
}

func TestFFXInvokerErrorOmitsConfigAndOutputPaths(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := "#!/bin/bash\nif [[ \"$*\" == *\"check-ssh-keys\"* ]]; then\n  exit 0\nfi\nexit 1\n"
	if err := os.WriteFile(ffxPath, []byte(script), os.ModePerm); err != nil {
		t.Fatalf("failed to write mock ffx tool: %s", err)
	}
	ctx := context.Background()
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "fuchsia-54b2-030e-eb1c", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXStrict)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}

	waitErr := ffx.TargetWait(ctx, "-t", "90")
	if waitErr == nil {
		t.Fatal("expected TargetWait to fail")
	}
	wantWaitPrefix := "FFX command failed (ffx --config log.level=debug target wait -t 90):"
	if !strings.HasPrefix(waitErr.Error(), wantWaitPrefix) {
		t.Errorf("TargetWait error = %q, want prefix %q", waitErr.Error(), wantWaitPrefix)
	}
	if strings.Contains(waitErr.Error(), tmpDir) || strings.Contains(waitErr.Error(), "fuchsia-54b2-030e-eb1c") {
		t.Errorf("TargetWait error should not contain temp paths or target nodename, got %q", waitErr.Error())
	}

	_, rebootErr := ffx.GetLastRebootReason(ctx)
	if rebootErr == nil {
		t.Fatal("expected GetLastRebootReason to fail")
	}
	wantRebootSubstr := "ffx target show failed: FFX command failed (ffx --machine json target show):"
	if !strings.Contains(rebootErr.Error(), wantRebootSubstr) {
		t.Errorf("GetLastRebootReason error = %q, want substring %q", rebootErr.Error(), wantRebootSubstr)
	}
	if strings.Contains(rebootErr.Error(), tmpDir) || strings.Contains(rebootErr.Error(), "fuchsia-54b2-030e-eb1c") {
		t.Errorf("GetLastRebootReason error should not contain temp paths or target nodename, got %q", rebootErr.Error())
	}
}

func TestExtractFFXMachineError(t *testing.T) {
	for _, tc := range []struct {
		name   string
		output string
		target string
		want   string
	}{
		{
			name:   "fastboot partition not big enough with NUL byte and Caused by",
			output: "{\"progress\":{\"stage\":\"boot_a\"}}\n{\"type\":\"unexpected\",\"code\":1,\"message\":\"Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough\\u0000\\n\\nCaused by:\\n    0: Failed to stage file\\n    1: Failed to upload boot_a\\n    2: failed to flash partition (boot_a): not big enough\\u0000\"}\n",
			target: "id:1A2B3C4D",
			want:   "Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough",
		},
		{
			name:   "target specification not found scrubs dynamic serial",
			output: `{"type":"user","code":1,"message":"Target specification \"id:11271J3DA017M8\" was not found. Use ` + "`ffx target list`" + ` to list known targets, and use a different matcher."}`,
			target: "id:11271J3DA017M8",
			want:   `Target specification "<target>" was not found. Use ` + "`ffx target list`" + ` to list known targets, and use a different matcher.`,
		},
		{
			name:   "fastboot write timeout",
			output: `{"type":"unexpected","code":1,"message":"Fastboot interface error: Fastboot send error: timed out writing to fastboot interface: Custom { kind: TimedOut, error: \"timed out\" }\n\nCaused by:\n    0: timed out"}`,
			target: "id:4B121JEAYW01WG",
			want:   `Fastboot interface error: Fastboot send error: timed out writing to fastboot interface: Custom { kind: TimedOut, error: "timed out" }`,
		},
		{
			name:   "finished envelope with BUG header",
			output: `{"finished":{"success":false,"error_message":"BUG: An internal command error occurred.\nError: Fastboot interface error: Fastboot error: Upload error: Could not write to interface\n    1. Fastboot error"}}`,
			target: "id:4B121JEAYW01WG",
			want:   "Fastboot interface error: Fastboot error: Upload error: Could not write to interface",
		},
		{
			name:   "bare serial stripped of id prefix is scrubbed",
			output: `{"type":"unexpected","code":1,"message":"Fastboot interface error: USB Interface creation error for serial 4B121JEAYW01WG: Resource busy (os error 16)"}`,
			target: "id:4B121JEAYW01WG",
			want:   "Fastboot interface error: USB Interface creation error for serial <target>: Resource busy (os error 16)",
		},
		{
			name:   "config error type is surfaced",
			output: `{"type":"config","code":1,"message":"Configuration error: missing required config value"}`,
			target: "id:4B121JEAYW01WG",
			want:   "Configuration error: missing required config value",
		},
		{
			name:   "non-error progress only returns empty",
			output: `{"type":"progress","message":"uploading boot_a"}`,
			target: "id:1A2B3C4D",
			want:   "",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := extractFFXMachineError(tc.output, tc.target); got != tc.want {
				t.Errorf("extractFFXMachineError() = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestFFXInvokerSurfacesMachineJSONError(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	script := "#!/bin/bash\nif [[ \"$*\" == *\"check-ssh-keys\"* ]]; then\n  exit 0\nfi\necho '{\"type\":\"unexpected\",\"code\":1,\"message\":\"Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough\\u0000\\n\\nCaused by:\\n    0: Failed to stage file\"}'\nexit 1\n"
	if err := os.WriteFile(ffxPath, []byte(script), os.ModePerm); err != nil {
		t.Fatalf("failed to write mock ffx tool: %s", err)
	}
	ctx := context.Background()
	sshKeys := SSHInfo{
		SshPriv: filepath.Join(tmpDir, "privKey"),
		SshPub:  filepath.Join(tmpDir, "pubKey"),
	}
	ffx, err := NewFFXInstance(ctx, ffxPath, tmpDir, []string{}, "id:11271J3DA017M8", &sshKeys, filepath.Join(tmpDir, "out"), UseFFXStrict)
	if err != nil {
		t.Fatalf("NewFFXInstance failed: %s", err)
	}

	flashErr := ffx.Flash(ctx, "id:11271J3DA017M8", "", "product_bundles.json", false)
	if flashErr == nil {
		t.Fatal("expected Flash to fail")
	}
	wantSubstr := "FFX command failed (ffx -v --machine json target flash --product-bundle product_bundles.json): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if flashErr.Error() != wantSubstr {
		t.Errorf("Flash error = %q, want %q", flashErr.Error(), wantSubstr)
	}

	bootErr := ffx.BootloaderBoot(ctx, "id:11271J3DA017M8", "product_bundles.json", false)
	if bootErr == nil {
		t.Fatal("expected BootloaderBoot to fail")
	}
	wantBoot := "FFX command failed (ffx target bootloader --product-bundle product_bundles.json boot): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if bootErr.Error() != wantBoot {
		t.Errorf("BootloaderBoot error = %q, want %q", bootErr.Error(), wantBoot)
	}

	waitErr := ffx.TargetWait(ctx, "-t", "90")
	if waitErr == nil {
		t.Fatal("expected TargetWait to fail")
	}
	wantWait := "FFX command failed (ffx --config log.level=debug target wait -t 90): Fastboot interface error: Failed to stage file Failed to upload boot_a: failed to flash partition (boot_a): not big enough: exit status 1"
	if waitErr.Error() != wantWait {
		t.Errorf("TargetWait error = %q, want %q", waitErr.Error(), wantWait)
	}
}
