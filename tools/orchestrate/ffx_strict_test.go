// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package orchestrate

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func createFakeFfx(t *testing.T, tmpDir string, script string) string {
	fakeFfx := filepath.Join(tmpDir, "fake_ffx")
	if err := os.WriteFile(fakeFfx, []byte(script), 0755); err != nil {
		t.Fatalf("failed to create fake ffx: %v", err)
	}
	// Prevent "text file busy" (ETXTBSY) by waiting until the kernel actually allows execution
	for i := 0; i < 20; i++ {
		cmd := exec.Command(fakeFfx)
		err := cmd.Run()
		if err == nil {
			break
		}
		if strings.Contains(err.Error(), "text file busy") {
			time.Sleep(10 * time.Millisecond)
			continue
		}
		t.Fatalf("failed to verify fake ffx execution: %v", err)
	}
	return fakeFfx
}

func TestFFXStrictClient_Isolation(t *testing.T) {
	t.Parallel()
	tmpDir1 := t.TempDir()
	tmpDir2 := t.TempDir()

	// We need a fake ffx executable path to satisfy NewFFXStrictClient
	fakeFfx1 := createFakeFfx(t, tmpDir1, "#!/bin/bash\nexit 0")
	fakeFfx2 := createFakeFfx(t, tmpDir2, "#!/bin/bash\nexit 0")

	ctx := context.Background()
	client1, err := NewFFXStrictClient(ctx, fakeFfx1, tmpDir1, "repo-1")
	if err != nil {
		t.Fatalf("NewFFXStrictClient 1 failed: %v", err)
	}
	defer client1.Close()

	client2, err := NewFFXStrictClient(ctx, fakeFfx2, tmpDir2, "repo-2")
	if err != nil {
		t.Fatalf("NewFFXStrictClient 2 failed: %v", err)
	}
	defer client2.Close()

	// Verify output directories are different
	if client1.outputsDir == client2.outputsDir {
		t.Errorf("Both clients use the same outputsDir: %s", client1.outputsDir)
	}

	// Verify SSH keys are generated in different directories (under their respective outputsDir)
	if client1.sshInfo.SshPriv == client2.sshInfo.SshPriv {
		t.Errorf("Both clients share the same SSH private key path: %s", client1.sshInfo.SshPriv)
	}
	if !strings.Contains(client1.sshInfo.SshPriv, tmpDir1) {
		t.Errorf("client1 private key %q is not under its tmpDir %q", client1.sshInfo.SshPriv, tmpDir1)
	}
	if !strings.Contains(client2.sshInfo.SshPriv, tmpDir2) {
		t.Errorf("client2 private key %q is not under its tmpDir %q", client2.sshInfo.SshPriv, tmpDir2)
	}
}

func TestFFXStrictClient_ApplyEnv(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createFakeFfx(t, tmpDir, "#!/bin/bash\nexit 0")

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	inputEnv := []string{
		"FOO=BAR",
		"FUCHSIA_DEVICE_ADDR=1.2.3.4",
		"PATH=/usr/bin:/bin",
	}

	gotEnv, err := client.ApplyEnv(inputEnv)
	if err != nil {
		t.Fatalf("ApplyEnv failed: %v", err)
	}

	// Verify FOO=BAR is preserved
	hasFoo := false
	for _, val := range gotEnv {
		if val == "FOO=BAR" {
			hasFoo = true
			break
		}
	}
	if !hasFoo {
		t.Errorf("Expected FOO=BAR in env, got: %v", gotEnv)
	}

	// Verify FUCHSIA_DEVICE_ADDR is removed
	for _, val := range gotEnv {
		if strings.HasPrefix(val, "FUCHSIA_DEVICE_ADDR=") {
			t.Errorf("FUCHSIA_DEVICE_ADDR should have been removed, but found: %s", val)
		}
	}

	// Verify XDG variables point to ffxStrictDir
	for _, xdgVar := range xdgEnvVars {
		expected := xdgVar + "=" + client.ffxStrictDir
		found := false
		for _, val := range gotEnv {
			if val == expected {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("Expected env to contain %q, got: %v", expected, gotEnv)
		}
	}

	// Verify PATH has openssh-portable prepended and no duplicates
	pathCount := 0
	hasSshPath := false
	for _, val := range gotEnv {
		if strings.HasPrefix(val, "PATH=") {
			pathCount++
			if strings.Contains(val, "openssh-portable/bin") && strings.HasSuffix(val, ":/usr/bin:/bin") {
				hasSshPath = true
			}
		}
	}
	if pathCount != 1 {
		t.Errorf("Expected exactly one PATH entry, got %d", pathCount)
	}
	if !hasSshPath {
		t.Errorf("Expected PATH to have openssh-portable prepended, got: %v", gotEnv)
	}
}

func TestFFXStrictClient_SetDefaultTarget(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createFakeFfx(t, tmpDir, "#!/bin/bash\nexit 0")

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	// Initially target should be empty, and FUCHSIA_NODENAME should not be set
	env, err := client.ApplyEnv([]string{})
	if err != nil {
		t.Fatalf("ApplyEnv failed: %v", err)
	}
	for _, val := range env {
		if strings.HasPrefix(val, "FUCHSIA_NODENAME=") {
			t.Errorf("FUCHSIA_NODENAME should not be set initially, but got: %s", val)
		}
	}

	// Set target
	target := "my-device"
	client.SetDefaultTarget(&target)

	env, err = client.ApplyEnv([]string{})
	if err != nil {
		t.Fatalf("ApplyEnv failed: %v", err)
	}
	hasTarget := false
	for _, val := range env {
		if val == "FUCHSIA_NODENAME=my-device" {
			hasTarget = true
			break
		}
	}
	if !hasTarget {
		t.Errorf("Expected FUCHSIA_NODENAME=my-device, got: %v", env)
	}

	// Unset target
	client.SetDefaultTarget(nil)
	env, err = client.ApplyEnv([]string{})
	if err != nil {
		t.Fatalf("ApplyEnv failed: %v", err)
	}
	for _, val := range env {
		if strings.HasPrefix(val, "FUCHSIA_NODENAME=") {
			t.Errorf("FUCHSIA_NODENAME should be unset, but got: %s", val)
		}
	}
}

func TestParsePort(t *testing.T) {
	t.Parallel()
	tests := []struct {
		name    string
		addrStr string
		want    int
		wantErr bool
	}{
		{"valid ipv4", "127.0.0.1:8080", 8080, false},
		{"valid ipv6", "[::1]:8080", 8080, false},
		{"invalid format", "8080", 0, true},
		{"not a number", "localhost:abc", 0, true},
		{"out of range high", "localhost:65536", 0, true},
		{"out of range low", "localhost:0", 0, true},
		{"negative port", "localhost:-1", 0, true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := parsePort(tt.addrStr)
			if (err != nil) != tt.wantErr {
				t.Errorf("parsePort() error = %v, wantErr %v", err, tt.wantErr)
				return
			}
			if got != tt.want {
				t.Errorf("parsePort() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestFFXStrictClient_RepositoryServer_Restart(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createSmartFakeFfx(t, tmpDir)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	target := "fake-target"
	client.SetDefaultTarget(&target)

	repoName := "test-repo"
	repoDir := filepath.Join(tmpDir, "repo")

	// 1. First Start
	err = client.RepositoryServerStart(ctx, repoName, repoDir, "localhost:0")
	if err != nil {
		t.Fatalf("First RepositoryServerStart failed: %v", err)
	}

	// 2. Stop
	err = client.RepositoryServerStop(ctx, repoName)
	if err != nil {
		t.Fatalf("RepositoryServerStop failed: %v", err)
	}

	// 3. Second Start
	err = client.RepositoryServerStart(ctx, repoName, repoDir, "localhost:0")
	if err != nil {
		t.Fatalf("Second RepositoryServerStart failed: %v", err)
	}
}

func TestFFXStrictClient_RepositoryServer_Restart_ExplicitPort(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createSmartFakeFfx(t, tmpDir)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	target := "fake-target"
	client.SetDefaultTarget(&target)

	repoName := "test-repo"
	repoDir := filepath.Join(tmpDir, "repo")

	// 1. First Start
	err = client.RepositoryServerStart(ctx, repoName, repoDir, "localhost:8084")
	if err != nil {
		t.Fatalf("First RepositoryServerStart failed: %v", err)
	}

	// 2. Stop
	err = client.RepositoryServerStop(ctx, repoName)
	if err != nil {
		t.Fatalf("RepositoryServerStop failed: %v", err)
	}

	// 3. Second Start
	err = client.RepositoryServerStart(ctx, repoName, repoDir, "localhost:8084")
	if err != nil {
		t.Fatalf("Second RepositoryServerStart failed: %v", err)
	}
}

func TestFFXStrictClient_RepositoryServer_Timeout(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createSmartFakeFfx(t, tmpDir)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "timeout-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	repoDir := filepath.Join(tmpDir, "repo")

	// Start should timeout after 5 seconds because the server hangs
	err = client.RepositoryServerStart(ctx, "timeout-repo", repoDir, "localhost:0")
	if err == nil {
		t.Fatalf("Expected timeout error, got nil")
	}
	if !strings.Contains(err.Error(), "timed out waiting for repository server to start") {
		t.Fatalf("Expected timeout error, got: %v", err)
	}

	// Verify the process is cleaned up and we can start a normal repo afterwards
	err = client.RepositoryServerStart(ctx, "test-repo", repoDir, "localhost:0")
	if err != nil {
		t.Fatalf("Failed to start normal repo after timeout cleanup: %v", err)
	}
	client.RepositoryServerStop(ctx, "test-repo")
}

func TestFFXStrictClient_RepositoryServer_StaleServer(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createSmartFakeFfx(t, tmpDir)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "stale-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	repoDir := filepath.Join(tmpDir, "repo")

	// is_list will return a stale entry first, but IsPackageServerRunning should ignore it and keep polling
	err = client.RepositoryServerStart(ctx, "stale-repo", repoDir, "localhost:0")
	if err != nil {
		t.Fatalf("RepositoryServerStart failed to ignore stale server: %v", err)
	}
	client.RepositoryServerStop(ctx, "stale-repo")
}

func TestFFXStrictClient_RepositoryServer_BadJson(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()
	fakeFfx := createSmartFakeFfx(t, tmpDir)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "bad-json-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	repoDir := filepath.Join(tmpDir, "repo")

	// is_list will return bad json, causing IsPackageServerRunning to fail cleanly
	// The polling loop logs the error and continues until the 5 second timeout.
	err = client.RepositoryServerStart(ctx, "bad-json-repo", repoDir, "localhost:0")
	if err == nil {
		t.Fatalf("Expected timeout error due to bad json, got nil")
	}
	if !strings.Contains(err.Error(), "timed out waiting for repository server to start") {
		t.Fatalf("Expected timeout error, got: %v", err)
	}
}

func createSmartFakeFfx(t *testing.T, tmpDir string) string {
	script := fmt.Sprintf(`#!/bin/bash
# Find log.dir in arguments to know where to write state
LOG_DIR=""
for ((i=1; i<=$#; i++)); do
    if [ "${!i}" = "-c" ]; then
        next=$((i+1))
        if [[ "${!next}" == log.dir=* ]]; then
            LOG_DIR="${!next#log.dir=}"
        fi
    fi
done

STATE_DIR=""
if [ -n "$LOG_DIR" ]; then
    STATE_DIR=$(dirname "$LOG_DIR")
else
    STATE_DIR="%s"
fi

STATE_FILE="$STATE_DIR/server_state.json"

# Check commands
is_start=false
is_list=false
is_stop=false

for arg in "$@"; do
    if [ "$arg" = "start" ]; then
        is_start=true
    elif [ "$arg" = "list" ]; then
        is_list=true
    elif [ "$arg" = "stop" ]; then
        is_stop=true
    fi
done

	if $is_start; then
		PORT="8083" # default port
		PORT_PATH=""
		for ((i=1; i<=$#; i++)); do
			if [ "${!i}" = "--address" ]; then
				next=$((i+1))
				PORT=$(echo "${!next}" | cut -d: -f2)
				if [ "$PORT" = "0" ]; then
					PORT="8083"
				fi
			elif [ "${!i}" = "--port-path" ]; then
				next=$((i+1))
				PORT_PATH="${!next}"
			fi
		done

		is_fail_later=false
		is_timeout=false
		REPO_NAME="test-repo"
		for ((i=1; i<=$#; i++)); do
			if [ "${!i}" = "--repository" ]; then
				next=$((i+1))
				REPO_NAME="${!next}"
			fi
		done
		for arg in "$@"; do
			if [ "$arg" = "fail-later-repo" ]; then
				is_fail_later=true
			elif [ "$arg" = "timeout-repo" ]; then
				is_timeout=true
			fi
		done

		echo "$REPO_NAME" > "$STATE_DIR/repo_name.txt"

		if $is_timeout; then
			while true; do sleep 1; done
		fi

		if [ -n "$PORT_PATH" ]; then
			echo "$PORT" > "$PORT_PATH"
		fi

		if $is_fail_later; then
			 echo "{\"port\": $PORT}" > "$STATE_FILE"
			 sleep 0.1
			 exit 1
		fi

		echo "{\"port\": $PORT}" > "$STATE_FILE"
		while true; do
			sleep 1
		done
elif $is_list; then
    if [ -f "$STATE_FILE" ]; then
        PORT=$(grep -o '"port": *[0-9]*' "$STATE_FILE" | grep -o '[0-9]*')
        REPO_NAME=""
        if [ -f "$STATE_DIR/repo_name.txt" ]; then
            REPO_NAME=$(cat "$STATE_DIR/repo_name.txt")
        fi

        if [ "$REPO_NAME" = "bad-json-repo" ]; then
            echo "{\"ok\":{\"data\":[invalid json"
            exit 0
        fi

        echo "{\"ok\":{\"data\":[{\"name\":\"test-repo\",\"address\":\"[::]:$PORT\"},{\"name\":\"fail-later-repo\",\"address\":\"[::]:$PORT\"},{\"name\":\"stale-repo\",\"address\":\"[::]:9999\"},{\"name\":\"stale-repo\",\"address\":\"[::]:$PORT\"}]}}"
    else
        echo '{"ok":{"data":[]}}'
    fi
    exit 0
elif $is_stop; then
    rm -f "$STATE_FILE"
    exit 0
fi

exit 0
`, tmpDir)
	return createFakeFfx(t, tmpDir, script)
}

func TestFFXStrictClient_Flash(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()

	argsFile := filepath.Join(tmpDir, "args.txt")
	script := fmt.Sprintf(`#!/bin/bash
echo "$@" >> %s
exit 0
`, argsFile)
	fakeFfx := createFakeFfx(t, tmpDir, script)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	err = client.Flash(ctx, "serial123", "path/to/product", "path/to/pubkey")
	if err != nil {
		t.Fatalf("Flash failed: %v", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("Failed to read args file: %v", err)
	}
	args := string(data)

	if !strings.Contains(args, "target flash") {
		t.Errorf("Expected 'target flash' in args, got: %s", args)
	}
	if !strings.Contains(args, "path/to/product") {
		t.Errorf("Expected 'path/to/product' in args, got: %s", args)
	}
	if !strings.Contains(args, "path/to/pubkey") {
		t.Errorf("Expected 'path/to/pubkey' in args, got: %s", args)
	}
}

func TestFFXStrictClient_EmuStart(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()

	argsFile := filepath.Join(tmpDir, "args.txt")
	script := fmt.Sprintf(`#!/bin/bash
echo "$@" >> %s
exit 0
`, argsFile)
	fakeFfx := createFakeFfx(t, tmpDir, script)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	err = client.EmuStart(ctx, "path/to/emu/product", "my-emu-name")
	if err != nil {
		t.Fatalf("EmuStart failed: %v", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("Failed to read args file: %v", err)
	}
	args := string(data)

	if !strings.Contains(args, "emu start") {
		t.Errorf("Expected 'emu start' in args, got: %s", args)
	}
	if !strings.Contains(args, "path/to/emu/product") {
		t.Errorf("Expected 'path/to/emu/product' in args, got: %s", args)
	}
	if !strings.Contains(args, "--name my-emu-name") {
		t.Errorf("Expected '--name my-emu-name' in args, got: %s", args)
	}
	if !strings.Contains(args, "--headless") {
		t.Errorf("Expected '--headless' in args, got: %s", args)
	}
}

func TestFFXStrictClient_EmuStop(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()

	argsFile := filepath.Join(tmpDir, "args.txt")
	script := fmt.Sprintf(`#!/bin/bash
echo "$@" >> %s
exit 0
`, argsFile)
	fakeFfx := createFakeFfx(t, tmpDir, script)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	err = client.EmuStop(ctx)
	if err != nil {
		t.Fatalf("EmuStop failed: %v", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("Failed to read args file: %v", err)
	}
	args := string(data)

	if !strings.Contains(args, "emu stop --all") {
		t.Errorf("Expected 'emu stop --all' in args, got: %s", args)
	}
}

func TestFFXStrictClient_RepositoryCreate(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()

	argsFile := filepath.Join(tmpDir, "args.txt")
	script := fmt.Sprintf(`#!/bin/bash
echo "$@" >> %s
exit 0
`, argsFile)
	fakeFfx := createFakeFfx(t, tmpDir, script)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	err = client.RepositoryCreate(ctx, "path/to/repo")
	if err != nil {
		t.Fatalf("RepositoryCreate failed: %v", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("Failed to read args file: %v", err)
	}
	args := string(data)

	if !strings.Contains(args, "repository create") {
		t.Errorf("Expected 'repository create' in args, got: %s", args)
	}
	if !strings.Contains(args, "path/to/repo") {
		t.Errorf("Expected 'path/to/repo' in args, got: %s", args)
	}
}

func TestFFXStrictClient_RepositoryPublish(t *testing.T) {
	t.Parallel()
	tmpDir := t.TempDir()

	argsFile := filepath.Join(tmpDir, "args.txt")
	script := fmt.Sprintf(`#!/bin/bash
echo "$@" >> %s
exit 0
`, argsFile)
	fakeFfx := createFakeFfx(t, tmpDir, script)

	ctx := context.Background()
	client, err := NewFFXStrictClient(ctx, fakeFfx, tmpDir, "test-repo")
	if err != nil {
		t.Fatalf("NewFFXStrictClient failed: %v", err)
	}
	defer client.Close()

	archives := []string{"path/to/arch1.far", "path/to/arch2.far"}
	err = client.RepositoryPublish(ctx, "path/to/repo", "path/to/product", archives)
	if err != nil {
		t.Fatalf("RepositoryPublish failed: %v", err)
	}

	data, err := os.ReadFile(argsFile)
	if err != nil {
		t.Fatalf("Failed to read args file: %v", err)
	}
	args := string(data)

	if !strings.Contains(args, "repository publish path/to/repo --product-bundle path/to/product") {
		t.Errorf("Expected 'repository publish path/to/repo --product-bundle path/to/product' in args, got: %s", args)
	}
	if !strings.Contains(args, "repository publish path/to/repo --package-archive path/to/arch1.far --package-archive path/to/arch2.far") {
		t.Errorf("Expected 'repository publish path/to/repo --package-archive path/to/arch1.far --package-archive path/to/arch2.far' in args, got: %s", args)
	}
}
