// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
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

func TestCheckDMCHealthCheckOutput(t *testing.T) {
	for _, tc := range []struct {
		name    string
		output  string
		wantErr bool
	}{
		{
			name:    "healthy single device",
			output:  `[{"nodename":"fuchsia-node-1","state":"healthy","status_message":"","dms_state":"AVAILABLE"}]`,
			wantErr: false,
		},
		{
			name:    "empty stdout rejected",
			output:  "  \n\t ",
			wantErr: true,
		},
		{
			name:    "empty JSON array rejected",
			output:  `[]`,
			wantErr: true,
		},
		{
			name:    "malformed JSON rejected",
			output:  `not-json`,
			wantErr: true,
		},
		{
			name:    "unhealthy state rejected",
			output:  `[{"nodename":"fuchsia-node-1","state":"unhealthy","status_message":"failed to place device in fastboot","dms_state":"REPAIRING"}]`,
			wantErr: true,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := checkDMCHealthCheckOutput([]byte(tc.output))
			if tc.wantErr && err == nil {
				t.Fatalf("checkDMCHealthCheckOutput(%q) = nil, want error", tc.output)
			}
			if !tc.wantErr && err != nil {
				t.Fatalf("checkDMCHealthCheckOutput(%q) = %v, want nil", tc.output, err)
			}
		})
	}
}

func TestPlaceInFastbootNonZeroExitIncludesStdout(t *testing.T) {
	origBackoff := fastbootRetryBackoff
	fastbootRetryBackoff = time.Millisecond
	t.Cleanup(func() { fastbootRetryBackoff = origBackoff })

	fakeDMC := filepath.Join(t.TempDir(), "dmc")
	script := "#!/bin/sh\nif [ \"$1\" = \"health-check\" ]; then\n  printf 'diagnostic failure detail'\n  exit 1\nfi\nexit 0\n"
	if err := os.WriteFile(fakeDMC, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DMC_PATH", fakeDMC)

	dev := &Device{
		config: DeviceConfig{
			Network: NetworkProperties{Nodename: "fuchsia-node-1"},
		},
	}
	err := dev.placeInFastboot(context.Background())
	if err == nil {
		t.Fatal("placeInFastboot() = nil, want error")
	}
	if !strings.Contains(err.Error(), "dmc health-check failed:") || !strings.Contains(err.Error(), "diagnostic failure detail") {
		t.Fatalf("placeInFastboot() error = %q, want stdout included in error message", err.Error())
	}
}

func TestPlaceInFastbootNonZeroExitIncludesStderr(t *testing.T) {
	origBackoff := fastbootRetryBackoff
	fastbootRetryBackoff = time.Millisecond
	t.Cleanup(func() { fastbootRetryBackoff = origBackoff })

	fakeDMC := filepath.Join(t.TempDir(), "dmc")
	script := "#!/bin/sh\nif [ \"$1\" = \"health-check\" ]; then\n  printf 'stderr diagnostic failure' >&2\n  exit 1\nfi\nexit 0\n"
	if err := os.WriteFile(fakeDMC, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("DMC_PATH", fakeDMC)

	dev := &Device{
		config: DeviceConfig{
			Network: NetworkProperties{Nodename: "fuchsia-node-1"},
		},
	}
	err := dev.placeInFastboot(context.Background())
	if err == nil {
		t.Fatal("placeInFastboot() = nil, want error")
	}
	if !strings.Contains(err.Error(), "dmc health-check failed:") || !strings.Contains(err.Error(), "stderr diagnostic failure") {
		t.Fatalf("placeInFastboot() error = %q, want stderr included in error message", err.Error())
	}
}
