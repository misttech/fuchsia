// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/botanist/targets"
	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	testrunnerconstants "go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"
	"golang.org/x/sync/errgroup"
)

type MockTarget struct {
	mu               sync.Mutex
	ffx              *targets.FFXInstance
	serialSocketPath string
	sshClientFunc    func(attempt int) (*sshutil.Client, error)
	sshClientCalls   int
}

func (m *MockTarget) TestConfig(expectsSSH bool) (any, error) { return nil, nil }
func (m *MockTarget) AddFFXPackageRepository(ctx context.Context, pkgRepoName string, pkgSrvPort int, useForward bool) (func(), error) {
	return func() {}, nil
}
func (m *MockTarget) AddPackageRepository(client *sshutil.Client, repoURL, blobURL string) error {
	return nil
}
func (m *MockTarget) CaptureSerialLog(filename string) error { return nil }
func (m *MockTarget) CaptureSyslog(client *sshutil.Client, filename string, pkgSrv *botanist.PackageServer) error {
	return nil
}
func (m *MockTarget) StopSyslog() {}
func (m *MockTarget) IPv6() (*net.IPAddr, error) {
	// Sleep to allow StartFFXMonitor to run before runAgainstTarget fails/returns
	time.Sleep(500 * time.Millisecond)
	addr, _ := net.ResolveIPAddr("ip6", "::1")
	return addr, nil
}
func (m *MockTarget) IPv4() (net.IP, error) {
	return net.ParseIP("127.0.0.1"), nil
}
func (m *MockTarget) Nodename() string           { return "mock-target" }
func (m *MockTarget) Serial() io.ReadWriteCloser { return nil }
func (m *MockTarget) SerialSocketPath() string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.serialSocketPath
}
func (m *MockTarget) SetConnectionTimeout(time.Duration) {}
func (m *MockTarget) ResolveIP() error                   { return nil }
func (m *MockTarget) SSHClient() (*sshutil.Client, error) {
	m.mu.Lock()
	m.sshClientCalls++
	attempt := m.sshClientCalls
	fn := m.sshClientFunc
	m.mu.Unlock()
	if fn != nil {
		return fn(attempt)
	}
	return nil, nil
}
func (m *MockTarget) SSHControlMasterPath() string { return "" }
func (m *MockTarget) SetupSSHControlMaster(ctx context.Context, sshKey, addr string) (func(), error) {
	return func() {}, nil
}
func (m *MockTarget) SSHControlMasterRunning() bool { return false }
func (m *MockTarget) SSHKey() string                { return "ssh-key" }
func (m *MockTarget) Start(ctx context.Context, args []string, pbPath string, isBootTest bool) error {
	return nil
}
func (m *MockTarget) StartSerialServer() error   { return nil }
func (m *MockTarget) Stop() error                { return nil }
func (m *MockTarget) Wait(context.Context) error { return nil }
func (m *MockTarget) SetFFX(ffx *targets.FFXInstance, env []string) {
	m.ffx = ffx
}
func (m *MockTarget) GetFFX() *targets.FFXInstance { return m.ffx }
func (m *MockTarget) UseFFXExperiment(exp botanist.Experiment) bool {
	return m.ffx.Experiments.Contains(exp)
}
func (m *MockTarget) UseProductBundles() bool { return false }
func (m *MockTarget) FFXEnv() []string        { return nil }
func (m *MockTarget) GetSharedData() string   { return "" }

func TestStartFFXMonitor(t *testing.T) {
	tmpDir := t.TempDir()
	ffxPath := filepath.Join(tmpDir, "ffx")
	// Create a mock ffx script that echoes arguments
	if err := os.WriteFile(ffxPath, []byte("#!/bin/bash\necho $@"), 0o755); err != nil {
		t.Fatal("failed to write mock ffx tool")
	}

	// Create test SSH keys
	sshPriv := filepath.Join(tmpDir, "ssh_priv")
	sshPub := filepath.Join(tmpDir, "ssh_pub")
	if err := os.WriteFile(sshPriv, []byte("PRIVATE KEY"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(sshPub, []byte("PUBLIC KEY"), 0o600); err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	ffxInstance, err := ffxutil.NewFFXInstance(ctx, ffxPath, tmpDir, nil, "target", &ffxutil.SSHInfo{SshPriv: sshPriv, SshPub: sshPub}, tmpDir, ffxutil.UseFFXLegacy)
	if err != nil {
		t.Fatalf("failed to create ffx instance: %s", err)
	}

	// Capture stdout
	rOut, wOut, _ := os.Pipe()
	ffxInstance.SetStdoutStderr(wOut, wOut)

	targetFFX := &targets.FFXInstance{FFXInstance: ffxInstance}
	target := &MockTarget{ffx: targetFFX}

	cmd := &RunCommand{
		expectsSSH:        true,
		productBundles:    "test-bundles",
		productBundleName: "test-bundle-name",
	}
	eg, ctx := errgroup.WithContext(ctx)
	cancel := func() {}

	// Set test out dir env var because StartFFXMonitor uses it
	os.Setenv(testrunnerconstants.TestOutDirEnvKey, tmpDir)
	defer os.Unsetenv(testrunnerconstants.TestOutDirEnvKey)

	experiments := make(botanist.Experiments)

	const (
		monitorName         = "ffx_monitor"
		aggregationFilename = "aggregation.json"
	)

	// Create an initial summary.json
	summaryPath := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, runtests.TestSummaryFilename)
	initialSummary := runtests.TestSummary{Tests: []runtests.TestDetails{}}
	summaryBytes, _ := json.Marshal(initialSummary)
	if err := os.WriteFile(summaryPath, summaryBytes, 0o600); err != nil {
		t.Fatalf("failed to write initial summary.json: %v", err)
	}

	// Run dispatchTests
	cmd.dispatchTests(ctx, cancel, eg, nil, []targets.FuchsiaTarget{target}, target, nil, "invalid-tests-path", experiments)

	// Wait for goroutines to complete
	_ = eg.Wait()

	wOut.Close()
	outBytes, _ := io.ReadAll(rOut)
	outStr := string(outBytes)

	// Check if monitor start command was issued with correct args
	if !strings.Contains(outStr, "monitor start") {
		t.Errorf("expected monitor start command, got: %s", outStr)
	}
	if !strings.Contains(outStr, "--log-file") {
		t.Errorf("expected --log-file arg, got: %s", outStr)
	}
	expectedLogFile := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, monitorName, "device.status.json")
	if !strings.Contains(outStr, expectedLogFile) {
		t.Errorf("expected log file %s, got output: %s", expectedLogFile, outStr)
	}
	if !strings.Contains(outStr, "--aggregations-file") {
		t.Errorf("expected --aggregations-file arg, got: %s", outStr)
	}
	expectedAggregationsFile := filepath.Join(tmpDir, cmd.testrunnerOptions.OutDir, monitorName, aggregationFilename)
	if !strings.Contains(outStr, expectedAggregationsFile) {
		t.Errorf("expected aggregations file %s, got output: %s", expectedAggregationsFile, outStr)
	}

	// Verify summary.json was updated
	data, err := os.ReadFile(summaryPath)
	if err != nil {
		t.Fatalf("failed to read summary.json: %v", err)
	}
	var summary runtests.TestSummary
	if err := json.Unmarshal(data, &summary); err != nil {
		t.Fatalf("failed to unmarshal summary.json: %v", err)
	}
	found := false
	for _, test := range summary.Tests {
		if test.Name == monitorName {
			found = true
			if test.OutputDir != monitorName {
				t.Errorf("expected OutputDir %s, got %s", monitorName, test.OutputDir)
			}
			if len(test.OutputFiles) != 1 || test.OutputFiles[0] != aggregationFilename {
				t.Errorf("expected OutputFiles [%s], got %v", aggregationFilename, test.OutputFiles)
			}
			break
		}
	}
	if !found {
		t.Error("ffx_monitor entry not found in summary.json")
	}
}

func TestGetBootTimeout(t *testing.T) {
	tests := []struct {
		name                   string
		bootTimeout            time.Duration
		testTimeoutScaleFactor int
		want                   time.Duration
	}{
		{
			name:                   "default unscaled",
			bootTimeout:            0,
			testTimeoutScaleFactor: 1,
			want:                   0,
		},
		{
			name:                   "default scaled by 2",
			bootTimeout:            0,
			testTimeoutScaleFactor: 2,
			want:                   2 * targets.DefaultConnectionTimeout,
		},
		{
			name:                   "custom unscaled",
			bootTimeout:            60 * time.Second,
			testTimeoutScaleFactor: 1,
			want:                   60 * time.Second,
		},
		{
			name:                   "custom scaled by 3",
			bootTimeout:            60 * time.Second,
			testTimeoutScaleFactor: 3,
			want:                   180 * time.Second,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			cmd := &RunCommand{
				bootupTimeout:          tc.bootTimeout,
				testTimeoutScaleFactor: tc.testTimeoutScaleFactor,
			}
			if got := cmd.getBootTimeout(); got != tc.want {
				t.Errorf("getBootTimeout() = %v, want %v", got, tc.want)
			}
		})
	}
}

func TestConnectToSSHRetryOnTimeout(t *testing.T) {
	timeoutErr := sshutil.ConnectionError{
		Err: fmt.Errorf("timed out trying to connect to ssh after 2m0s: %w", context.DeadlineExceeded),
	}

	t.Run("recovers via serial reboot on timeout", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		socketPath := targets.CreateSocketPath("serial")
		t.Cleanup(func() {
			os.Remove(socketPath)
		})
		listener, err := net.Listen("unix", socketPath)
		if err != nil {
			t.Fatalf("net.Listen(unix, %q) failed: %v", socketPath, err)
		}
		t.Cleanup(func() {
			listener.Close()
		})

		serialCmdCh := make(chan string, 1)
		go func() {
			for {
				conn, err := listener.Accept()
				if err != nil {
					return
				}
				_, _ = conn.Write([]byte("$ "))
				var received strings.Builder
				buf := make([]byte, 256)
				for {
					n, readErr := conn.Read(buf)
					if n > 0 {
						received.Write(buf[:n])
						if strings.Contains(received.String(), "dm reboot") {
							serialCmdCh <- received.String()
							break
						}
					}
					if readErr != nil {
						serialCmdCh <- received.String()
						break
					}
				}
				conn.Close()
			}
		}()

		target := &MockTarget{
			serialSocketPath: socketPath,
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				if attempt == 1 {
					return nil, timeoutErr
				}
				return nil, nil
			},
		}

		cmd := &RunCommand{}
		if _, err := cmd.connectToSSH(context.Background(), target); err != nil {
			t.Fatalf("expected connectToSSH to succeed after serial reboot, got: %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected 2 SSHClient attempts, got %d", sshCalls)
		}

		select {
		case gotRebootCmd := <-serialCmdCh:
			if !strings.Contains(gotRebootCmd, "dm reboot") {
				t.Errorf("expected serial reboot command 'dm reboot', got %q", gotRebootCmd)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("timed out waiting for serial reboot command")
		}
	})

	t.Run("falls back to dmc power cycle when serial reboot fails", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcArgsFile := filepath.Join(tmpDir, "dmc_args.txt")
		dmcPath := filepath.Join(tmpDir, "dmc")
		dmcScript := fmt.Sprintf("#!/bin/sh\necho \"$@\" > %q\n", dmcArgsFile)
		if err := os.WriteFile(dmcPath, []byte(dmcScript), 0o755); err != nil {
			t.Fatalf("failed to write mock dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			serialSocketPath: "/tmp/nonexistent-%s.sock",
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				if attempt == 1 {
					return nil, timeoutErr
				}
				return nil, nil
			},
		}

		var logBuf bytes.Buffer
		logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.WarningLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

		cmd := &RunCommand{}
		if _, err := cmd.connectToSSH(logCtx, target); err != nil {
			t.Fatalf("expected connectToSSH to succeed after dmc recovery, got: %v", err)
		}
		if strings.Contains(logBuf.String(), "failed to open serial socket connection") || strings.Contains(logBuf.String(), "failed to find cursor") {
			t.Errorf("expected serial socket failure not to be logged when dmc recovery succeeds, got: %s", logBuf.String())
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected 2 SSHClient attempts, got %d", sshCalls)
		}

		dmcArgsBytes, err := os.ReadFile(dmcArgsFile)
		if err != nil {
			t.Fatalf("expected dmc to be invoked, failed to read %s: %v", dmcArgsFile, err)
		}
		wantDMCArgs := "set-power-state --nodename mock-target --state cycle"
		if got := strings.TrimSpace(string(dmcArgsBytes)); got != wantDMCArgs {
			t.Errorf("dmc args = %q, want %q", got, wantDMCArgs)
		}
	})

	t.Run("aborts when all recovery mechanisms fail", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcPath := filepath.Join(tmpDir, "dmc")
		if err := os.WriteFile(dmcPath, []byte("#!/bin/sh\nexit 1\n"), 0o755); err != nil {
			t.Fatalf("failed to write failing dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			serialSocketPath: "/tmp/nonexistent-%s.sock",
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		var logBuf bytes.Buffer
		logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.WarningLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(logCtx, target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("expected context.DeadlineExceeded when recovery fails, got: %v", err)
		}
		if strings.Contains(logBuf.String(), "%!s(MISSING)") {
			t.Errorf("expected warning log to format %% safely without %%!s(MISSING), got: %s", logBuf.String())
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Fatalf("expected 1 SSHClient attempt when recovery fails, got %d", sshCalls)
		}
	})

	t.Run("aborts when no recovery mechanism is available", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("expected context.DeadlineExceeded when no recovery mechanism is available, got: %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Fatalf("expected 1 SSHClient attempt when no recovery mechanism is available, got %d", sshCalls)
		}
	})

	t.Run("aborts after second timeout without further retries", func(t *testing.T) {
		tmpDir := t.TempDir()
		dmcPath := filepath.Join(tmpDir, "dmc")
		if err := os.WriteFile(dmcPath, []byte("#!/bin/sh\nexit 0\n"), 0o755); err != nil {
			t.Fatalf("failed to write mock dmc script: %v", err)
		}
		t.Setenv(testrunnerconstants.DMCPathEnvKey, dmcPath)

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, context.DeadlineExceeded) {
			t.Errorf("expected context.DeadlineExceeded error after retry failure, got %v", err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 2 {
			t.Fatalf("expected exactly 2 SSHClient attempts, got %d", sshCalls)
		}
	})

	t.Run("does not retry on non-timeout error", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		nonTimeoutErr := errors.New("SSHClient() failed; no ssh keys provided")
		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				return nil, nonTimeoutErr
			},
		}

		cmd := &RunCommand{}
		_, err := cmd.connectToSSH(context.Background(), target)
		if !errors.Is(err, nonTimeoutErr) {
			t.Errorf("expected error %v, got %v", nonTimeoutErr, err)
		}

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Errorf("expected 1 SSHClient attempt for non-timeout error, got %d", sshCalls)
		}
	})

	t.Run("does not retry when parent context is canceled", func(t *testing.T) {
		t.Setenv(testrunnerconstants.DMCPathEnvKey, "")

		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()

		target := &MockTarget{
			sshClientFunc: func(attempt int) (*sshutil.Client, error) {
				cancel()
				return nil, timeoutErr
			},
		}

		cmd := &RunCommand{}
		_, _ = cmd.connectToSSH(ctx, target)

		target.mu.Lock()
		sshCalls := target.sshClientCalls
		target.mu.Unlock()
		if sshCalls != 1 {
			t.Errorf("expected 1 SSHClient attempt when parent context is canceled, got %d", sshCalls)
		}
	})
}

func TestExecuteSkipSetupPropagatesTestrunnerError(t *testing.T) {
	var logBuf bytes.Buffer
	logCtx := logger.WithLogger(context.Background(), logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorNever), &logBuf, &logBuf, ""))

	cmd := &RunCommand{
		skipSetup: true,
		timeout:   10 * time.Minute,
	}
	err := cmd.execute(logCtx, []string{filepath.Join(t.TempDir(), "nonexistent-tests.json")})
	if err == nil {
		t.Fatal("expected execute to fail for nonexistent tests.json")
	}
	if strings.Contains(err.Error(), "testrunner with flags:") {
		t.Errorf("expected error not to contain raw testrunnerOptions dump, got: %v", err)
	}
	if !strings.Contains(logBuf.String(), "testrunner with flags:") {
		t.Errorf("expected debug log to contain testrunner flags, got: %s", logBuf.String())
	}
}
