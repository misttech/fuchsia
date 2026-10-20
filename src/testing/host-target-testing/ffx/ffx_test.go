// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffx

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	botanistconstants "go.fuchsia.dev/fuchsia/tools/botanist/constants"
)

// createScript returns the path to a bash script that output the given content.
func createScript(t *testing.T, contents string) string {
	name := filepath.Join(t.TempDir(), "ffxtool.sh")
	contents = `#!/bin/bash
	echo '` + contents + `'`
	if err := os.WriteFile(name, []byte(contents), 0o700); err != nil {
		t.Fatal(err)
	}
	return name
}

func TestTargetListEmpty(t *testing.T) {
	data, err := json.Marshal([]TargetEntry{})
	if err != nil {
		t.Fatalf("Failed to marshal: %s", err)
	}

	ffxtoolScript := createScript(t, string(data))

	runDir := NewRunDir(filepath.Join(t.TempDir(), "ffx-run-dir"))
	ffx, err := NewFFXTool(ffxtoolScript, runDir)
	if err != nil {
		t.Fatalf("Failed to create ffx tool: %s", err)
	}
	entries, err := ffx.TargetList(context.Background(), "", 0)
	if err != nil {
		t.Fatalf("Failed to run target list: %s", err)
	}
	if len(entries) != 0 {
		t.Fatalf("entries not empty: %v", entries)
	}
}

func TestTargetList(t *testing.T) {
	expected_entries := []TargetEntry{
		{NodeName: "1", Addresses: []TargetAddress{{Type: "Ip", IP: "127.0.0.1"}}, TargetState: "Product"},
		{NodeName: "2", Addresses: []TargetAddress{{Type: "Ip", IP: "127.0.0.2"}}, TargetState: "Product"},
		{NodeName: "fuchsia-5254-475e-82ef", Addresses: []TargetAddress{{Type: "Ip", IP: "fe80::9bf7:2e3:c4f8:9638%qemu"}}, TargetState: "Product"},
	}
	data, err := json.Marshal(expected_entries)
	if err != nil {
		t.Fatalf("Failed to marshal: %s", err)
	}

	ffxtoolScript := createScript(t, string(data))

	runDir := NewRunDir(filepath.Join(t.TempDir(), "ffx-run-dir"))
	ffx, err := NewFFXTool(ffxtoolScript, runDir)
	if err != nil {
		t.Fatalf("Failed to create ffx tool: %s", err)
	}
	entries, err := ffx.TargetList(context.Background(), "", 0)
	if err != nil {
		t.Fatalf("Failed to run target list: %s", err)
	}
	if diff := cmp.Diff(entries, expected_entries); diff != "" {
		t.Fatalf("unexpected entries, diff:\n%s", diff)
	}

	entries, err = ffx.TargetListForNode(context.Background(), "1")
	if err != nil {
		t.Fatalf("Failed to run target list: %s", err)
	}
	if diff := cmp.Diff(entries, expected_entries[:1]); diff != "" {
		t.Fatalf("unexpected entries, diff:\n%s", diff)
	}

	target, err := ffx.WaitForTarget(context.Background(), "127.0.0.1")
	if err != nil {
		t.Fatalf("Failed to run target list: %s", err)
	}
	if target.NodeName != "1" {
		t.Fatalf("unexpected device name, expected 1, got %s", target.NodeName)
	}
}

func TestTargetListStrict(t *testing.T) {
	expectedEntries := []TargetEntry{
		{NodeName: "1", Addresses: []TargetAddress{{Type: "Ip", IP: "127.0.0.1"}}, TargetState: "Product"},
		{NodeName: "2", Addresses: []TargetAddress{{Type: "Ip", IP: "127.0.0.2"}}, TargetState: "Product"},
	}
	data, err := json.Marshal(expectedEntries)
	if err != nil {
		t.Fatalf("Failed to marshal: %s", err)
	}

	ffxtoolScript := createScript(t, string(data))

	runDir := NewRunDir(filepath.Join(t.TempDir(), "ffx-run-dir"))

	// We must call newFfxStrict directly because NewFFXToolForVersion still defaults to daemon mode.
	ffxStrict, err := newFfxStrict(context.Background(), ffxtoolScript, runDir, "")
	if err != nil {
		t.Fatalf("Failed to create ffx strict: %s", err)
	}
	entries, err := ffxStrict.TargetList(context.Background(), "", 0)
	if err != nil {
		t.Fatalf("Failed to run target list: %s", err)
	}
	if diff := cmp.Diff(entries, expectedEntries); diff != "" {
		t.Fatalf("unexpected entries, diff:\n%s", diff)
	}
}

func TestCloseDaemon(t *testing.T) {
	ffxtoolScript := createScript(t, "")
	runDir := RunDir{path: t.TempDir()}
	ffx, err := newFfxDaemon(context.Background(), ffxtoolScript, runDir, "")
	if err != nil {
		t.Fatal(err)
	}
	if err := ffx.Close(context.Background()); err != nil {
		t.Fatalf("Close failed: %s", err)
	}
}

func TestCloseStrict(t *testing.T) {
	ffxtoolScript := createScript(t, "")
	runDir := RunDir{path: t.TempDir()}
	ffxStrict, err := newFfxStrict(context.Background(), ffxtoolScript, runDir, "")
	if err != nil {
		t.Fatal(err)
	}
	if err := ffxStrict.Close(context.Background()); err != nil {
		t.Fatalf("Close failed: %s", err)
	}
}

// createHostToolsDir creates a fake botanist host tools directory whose ffx
// is a bash script that appends its arguments, one per line, to argsFile.
func createHostToolsDir(t *testing.T, argsFile string) string {
	hostTools := t.TempDir()
	contents := `#!/bin/bash
	printf '%s\n' "$@" >> '` + argsFile + `'`
	if err := os.WriteFile(filepath.Join(hostTools, "ffx"), []byte(contents), 0o700); err != nil {
		t.Fatal(err)
	}
	return hostTools
}

func TestNotifyIntentionalDisconnectNoMonitor(t *testing.T) {
	for _, unset := range []string{
		botanistconstants.FFXSharedDataEnvKey,
		botanistconstants.HostToolsEnvKey,
	} {
		t.Run("no "+unset, func(t *testing.T) {
			t.Setenv(botanistconstants.FFXSharedDataEnvKey, t.TempDir())
			t.Setenv(botanistconstants.HostToolsEnvKey, t.TempDir())
			t.Setenv(unset, "")
			if HasMonitor() {
				t.Fatalf("HasMonitor() = true when %s is unset", unset)
			}
			if err := NotifyIntentionalDisconnect(context.Background(), "fuchsia-1234"); err == nil {
				t.Fatalf("expected an error when %s is unset", unset)
			}
		})
	}
}

func TestNotifyIntentionalDisconnect(t *testing.T) {
	sharedData := filepath.Join(t.TempDir(), "botanist-shared")
	argsFile := filepath.Join(t.TempDir(), "args.txt")
	hostTools := createHostToolsDir(t, argsFile)
	t.Setenv(botanistconstants.FFXSharedDataEnvKey, sharedData)
	t.Setenv(botanistconstants.HostToolsEnvKey, hostTools)

	if !HasMonitor() {
		t.Fatal("HasMonitor() = false")
	}
	if err := NotifyIntentionalDisconnect(context.Background(), "fuchsia-1234"); err != nil {
		t.Fatalf("NotifyIntentionalDisconnect failed: %s", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatal(err)
	}
	args := strings.Split(strings.TrimSpace(string(data)), "\n")

	want := []string{
		"-o", "stderr", "--log-level", "error", "--strict", "--machine", "json",
		"-c", "ffx.subtool-search-paths=" + hostTools,
		"-c", "shared_data=" + sharedData,
		"monitor", "intentional-disconnect", "--nodename", "fuchsia-1234",
	}
	if diff := cmp.Diff(want, args); diff != "" {
		t.Fatalf("unexpected args, diff:\n%s", diff)
	}
}

func TestNotifyIntentionalDisconnectFailure(t *testing.T) {
	hostTools := t.TempDir()
	contents := "#!/bin/bash\necho 'reading port file: No such file' >&2\nexit 1"
	if err := os.WriteFile(filepath.Join(hostTools, "ffx"), []byte(contents), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv(botanistconstants.FFXSharedDataEnvKey, t.TempDir())
	t.Setenv(botanistconstants.HostToolsEnvKey, hostTools)

	err := NotifyIntentionalDisconnect(context.Background(), "fuchsia-1234")
	if err == nil {
		t.Fatal("expected an error when ffx fails")
	}
	if !strings.Contains(err.Error(), "reading port file") {
		t.Fatalf("error should include ffx output, got: %v", err)
	}
}

func TestMatchTarget(t *testing.T) {
	entry := TargetEntry{
		NodeName: "my-node",
		Serial:   "12345",
		Addresses: []TargetAddress{
			{Type: "Ip", IP: "192.168.1.5", SSHPort: 2222},
		},
	}

	tests := []struct {
		target string
		want   bool
	}{
		{"", true},
		{"my-node", true},
		{"12345", true},
		{"192.168.1.5", true},
		{"192.168.1.5:2222", true},
		{"other-node", false},
		{"192.168.1.6", false},
	}

	for _, tc := range tests {
		got := matchTarget(entry, tc.target)
		if got != tc.want {
			t.Errorf("matchTarget(entry, %q) = %v; want %v", tc.target, got, tc.want)
		}
	}
}
