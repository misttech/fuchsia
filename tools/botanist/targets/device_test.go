// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bytes"
	"encoding/base64"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestLoadConfigs(t *testing.T) {
	tests := []struct {
		name        string
		jsonStr     string
		expectedLen int
		expectErr   bool
	}{
		// Valid configs.
		{"ValidConfig", `[{"nodename":"upper-drank-wick-creek"},{"nodename":"siren-swoop-wick-hasty"}]`, 2, false},
		// Invalid configs.
		{"InvalidConfig", `{{"nodename":"upper-drank-wick-creek"},{"nodename":"siren-swoop-wick-hasty"}}`, 0, true},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			configs, err := LoadDeviceConfigs(mkTempFile(t, test.jsonStr))
			if test.expectErr && err == nil {
				t.Error("expected errors; no errors found")
			}
			if !test.expectErr && err != nil {
				t.Errorf("expected no errors; found error %s", err)
			}
			if len(configs) != test.expectedLen {
				t.Errorf("expected %d nodes; found %d", test.expectedLen, len(configs))
			}
		})
	}
}

// mkTempFile returns a new temporary file with the specified content that will
// be cleaned up automatically.
func mkTempFile(t *testing.T, content string) string {
	name := filepath.Join(t.TempDir(), "foo")
	if err := os.WriteFile(name, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
	return name
}

func TestIrisAuthorizedKeysCmds(t *testing.T) {
	t.Run("EmptyKeys", func(t *testing.T) {
		cmds := irisAuthorizedKeysCmds(nil)
		if len(cmds) != 0 {
			t.Errorf("expected 0 cmds, got %d", len(cmds))
		}
	})

	t.Run("ValidKeysChunked", func(t *testing.T) {
		keys := []byte("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG6aXn5K9V9... user@host\n")
		cmds := irisAuthorizedKeysCmds(keys)
		if len(cmds) == 0 {
			t.Fatal("expected non-empty cmds")
		}

		// First command must be oem cmdline set.
		expectedSet := []string{"oem", "cmdline", "set"}
		if !reflect.DeepEqual(cmds[0], expectedSet) {
			t.Errorf("expected first command %v, got %v", expectedSet, cmds[0])
		}

		// Check each subsequent command length <= 64 bytes when formatted.
		var reassembledBase64 strings.Builder
		for i, cmd := range cmds[1:] {
			if len(cmd) != 4 || cmd[0] != "oem" || cmd[1] != "cmdline" || cmd[2] != "add" {
				t.Fatalf("cmd %d improperly formatted: %v", i+1, cmd)
			}
			fullCmdStr := fmt.Sprintf("oem cmdline add %s", cmd[3])
			if len(fullCmdStr) > 64 {
				t.Errorf("cmd %d length %d exceeds max 64: %q", i+1, len(fullCmdStr), fullCmdStr)
			}
			const prefix = "iris.ssh_creds="
			if !strings.HasPrefix(cmd[3], prefix) {
				t.Fatalf("cmd %d arg %q missing prefix %q", i+1, cmd[3], prefix)
			}
			reassembledBase64.WriteString(strings.TrimPrefix(cmd[3], prefix))
		}

		decoded, err := base64.StdEncoding.DecodeString(reassembledBase64.String())
		if err != nil {
			t.Fatalf("failed to decode reassembled base64: %v", err)
		}
		if !bytes.Equal(decoded, keys) {
			t.Errorf("decoded keys mismatch: got %q, want %q", string(decoded), string(keys))
		}
	})
}
