// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package orchestrate

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	ffx "go.fuchsia.dev/fuchsia/tools/orchestrate/ffx"
	utils "go.fuchsia.dev/fuchsia/tools/orchestrate/utils"
)

// FFXStrictClient adapts ffxutil.FFXInstance to implement FFXClient in strict mode.
type FFXStrictClient struct {
	// TODO(jcecil): Remove this embedding once strict mode is fully implemented.
	*ffx.Ffx // Embed legacy client to fallback on unimplemented methods

	// The following fields are read-only after initialization.
	ffxInst      *ffxutil.FFXInstance
	outputsDir   string
	ffxStrictDir string
	sshInfo      *ffxutil.SSHInfo
	sshDir       string
	isTemp       bool

	mu           sync.Mutex // protects mutable server lifecycle fields below and target state
	serverCtx    context.Context
	serverCancel context.CancelFunc
	serverDone   chan struct{}
	serverErr    error
	repoPort     int
}

// NewFFXStrictClient creates a new FFXStrictClient.
func NewFFXStrictClient(ctx context.Context, ffxPath, outputsDir, repoName string) (*FFXStrictClient, error) {
	if ffxPath == "" {
		return nil, fmt.Errorf("ffxPath must not be empty")
	}

	isTemp := false
	if outputsDir == "" {
		var err error
		outputsDir, err = os.MkdirTemp("", "orchestrate-ffx-strict")
		if err != nil {
			return nil, fmt.Errorf("failed to create temp dir for ffx strict: %w", err)
		}
		isTemp = true
	}

	var success bool
	defer func() {
		if !success && isTemp {
			os.RemoveAll(outputsDir)
		}
	}()

	absOutputsDir, err := filepath.Abs(outputsDir)
	if err != nil {
		return nil, fmt.Errorf("failed to get absolute path for outputsDir %q: %w", outputsDir, err)
	}

	ffxStrictDir := filepath.Join(absOutputsDir, "ffx_strict")

	sshDir := filepath.Join(ffxStrictDir, "ssh")
	if err := os.MkdirAll(sshDir, 0700); err != nil {
		return nil, fmt.Errorf("failed to create ssh directory: %w", err)
	}

	sshPriv := filepath.Join(sshDir, "fuchsia_ed25519")
	sshInfo := &ffxutil.SSHInfo{
		SshPriv: sshPriv,
	}

	extraConfigs := ffxutil.ConfigSettings{
		Level: "global",
		Settings: map[string]any{
			"fastboot.flash.timeout_rate":     "1",
			"fastboot.flash.min_timeout_secs": "600",
			"fastboot.usb.disabled":           true,
			"proactive_log.enabled":           false,
			"discovery.mdns.enabled":          false,
			"overnet.cso":                     "only",
			"repository.default":              repoName,
			"repository.server.enabled":       false,
			"log.level":                       "Debug",
			"daemon.autostart":                false,
		},
	}

	ffxInst, err := ffxutil.NewFFXInstance(
		ctx,
		ffxPath,
		"",         // processDir
		[]string{}, // env
		"",         // target
		sshInfo,
		ffxStrictDir,
		ffxutil.UseFFXStrict,
		extraConfigs,
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create FFXInstance: %w", err)
	}

	// NOTE: We initialize the embedded legacy Ffx client with IsolateDir = ffxStrictDir.
	// Because Go embedding doesn't dynamically dispatch methods like ApplyEnv, unimplemented
	// FFXClient methods falling back to this legacy instance will use its legacy configuration.
	// Matching the IsolateDir ensures fallback commands run in the same sandbox as the strict instance.
	ffxOpt := &ffx.Option{
		ExePath:    ffxPath,
		LogDir:     outputsDir,
		IsolateDir: ffxStrictDir,
	}
	legacyFfx, err := ffx.New(ctx, ffxOpt)
	if err != nil {
		return nil, fmt.Errorf("failed to create legacy FFX: %w", err)
	}

	wd, err := os.Getwd()
	if err != nil {
		return nil, fmt.Errorf("failed to get working directory: %w", err)
	}
	sshBinDir := filepath.Join(wd, "openssh-portable", "bin")

	success = true
	return &FFXStrictClient{
		Ffx:          legacyFfx,
		ffxInst:      ffxInst,
		outputsDir:   absOutputsDir,
		ffxStrictDir: ffxStrictDir,
		sshInfo:      sshInfo,
		sshDir:       sshBinDir,
		isTemp:       isTemp,
	}, nil
}

func (c *FFXStrictClient) Close() error {
	c.mu.Lock()
	cancel := c.serverCancel
	doneChan := c.serverDone
	c.mu.Unlock()

	if cancel != nil {
		cancel()
		if doneChan != nil {
			<-doneChan
		}
		c.mu.Lock()
		c.serverCancel = nil
		c.serverCtx = nil
		c.serverDone = nil
		c.serverErr = nil
		c.repoPort = 0
		c.mu.Unlock()
	}

	// DO NOT call c.Ffx.Close() because it unconditionally deletes its IsolateDir.
	// Since we set IsolateDir to ffxStrictDir, calling c.Ffx.Close() would wipe
	// the entire ffx_strict sandbox, preventing log collection by Bazel.
	err := c.ffxInst.Stop()
	if c.isTemp {
		if rmErr := os.RemoveAll(c.outputsDir); rmErr != nil {
			if err != nil {
				return fmt.Errorf("removing temp dir %q: %w (stop err: %v)", c.outputsDir, rmErr, err)
			}
			return fmt.Errorf("removing temp dir %q: %w", c.outputsDir, rmErr)
		}
	}
	return err
}

func (c *FFXStrictClient) SetDefaultTarget(target *string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.Ffx.SetDefaultTarget(target)
	if target != nil {
		c.ffxInst.SetTarget(*target)
	} else {
		c.ffxInst.SetTarget("")
	}
}

func (c *FFXStrictClient) Flash(ctx context.Context, fastbootSerial, productDir, pubKeyPath string) error {
	if err := c.ffxInst.Flash(ctx, fastbootSerial, pubKeyPath, productDir, false); err != nil {
		return fmt.Errorf("ffx flash failed: %w", err)
	}
	return nil
}

func (c *FFXStrictClient) EmuStart(ctx context.Context, productDir, name string) error {
	args := []string{
		"emu", "start", productDir,
		"--net", "user",
		"--headless",
		"--startup-timeout", "300",
		"--name", name,
	}
	if err := c.ffxInst.Run(ctx, args...); err != nil {
		return fmt.Errorf("emu start failed: %w", err)
	}
	return nil
}

func (c *FFXStrictClient) EmuStop(ctx context.Context) error {
	if err := c.ffxInst.Run(ctx, "emu", "stop", "--all"); err != nil {
		return fmt.Errorf("emu stop failed: %w", err)
	}
	return nil
}

func (c *FFXStrictClient) RepositoryCreate(ctx context.Context, repoDir string) error {
	if err := c.ffxInst.Run(ctx, "repository", "create", repoDir); err != nil {
		return fmt.Errorf("repository create failed: %w", err)
	}
	return nil
}

func (c *FFXStrictClient) RepositoryPublish(ctx context.Context, repoDir string, productDir string, packageArchives []string) error {
	if err := c.ffxInst.Run(ctx, "repository", "publish", repoDir, "--product-bundle", productDir); err != nil {
		return fmt.Errorf("repository publish (product-bundle) failed: %w", err)
	}
	if len(packageArchives) > 0 {
		args := []string{"repository", "publish", repoDir}
		for _, far := range packageArchives {
			args = append(args, "--package-archive", far)
		}
		if err := c.ffxInst.Run(ctx, args...); err != nil {
			return fmt.Errorf("repository publish (package-archives) failed: %w", err)
		}
	}
	return nil
}

func parsePort(addrStr string) (int, error) {
	_, portStr, err := net.SplitHostPort(addrStr)
	if err != nil {
		return 0, err
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return 0, err
	}
	if port <= 0 || port > 65535 {
		return 0, fmt.Errorf("port %d out of range (must be > 0 and <= 65535)", port)
	}
	return port, nil
}

func (c *FFXStrictClient) RepositoryServerStart(ctx context.Context, repoName, repoDir, address string) error {
	host, portStr, err := net.SplitHostPort(address)
	if err != nil {
		return fmt.Errorf("invalid address %q: %w", address, err)
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return fmt.Errorf("invalid port in address %q: %w", address, err)
	}
	if port < 0 || port > 65535 {
		return fmt.Errorf("port %d out of range (must be >= 0 and <= 65535)", port)
	}
	if host == "" {
		host = "::"
	}

	f, err := os.CreateTemp(c.ffxStrictDir, "repository_port_*.txt")
	if err != nil {
		return fmt.Errorf("failed to create temp port file: %w", err)
	}
	portPath := f.Name()
	f.Close()
	defer os.Remove(portPath)

	c.mu.Lock()

	if c.serverCancel != nil {
		c.mu.Unlock()
		return fmt.Errorf("repository server is already running or starting")
	}

	if port != 0 {
		c.repoPort = port
	}

	serverCtx, serverCancel := context.WithCancel(context.Background())
	c.serverCtx = serverCtx
	c.serverCancel = serverCancel
	c.serverDone = make(chan struct{})
	c.serverErr = nil

	serverDone := c.serverDone

	go func() {
		trustedRoot := filepath.Join(repoDir, "repository", "9.root.json")

		args := []string{"repository", "server", "start", "--foreground", "--address", net.JoinHostPort(host, portStr),
			"--repository", repoName, "--repo-path", repoDir,
			"--trusted-root", trustedRoot,
			"--alias", "fuchsia.com", "--alias", "chromium.org",
			"--no-device",
			"--refresh-metadata",
			"--port-path", portPath,
		}
		cmd := c.ffxInst.Command(args...)
		err := c.ffxInst.RunCommand(serverCtx, cmd)

		c.mu.Lock()
		if c.serverCtx == serverCtx {
			if !errors.Is(serverCtx.Err(), context.Canceled) {
				if err != nil {
					c.serverErr = fmt.Errorf("repository server exited: %w", err)
				} else {
					c.serverErr = fmt.Errorf("repository server exited unexpectedly")
				}
			}
			serverCancel()
			c.serverCancel = nil
			c.serverCtx = nil
			c.serverDone = nil
			c.repoPort = 0
		}
		c.mu.Unlock()
		close(serverDone)
	}()

	c.mu.Unlock()

	started := false
	defer func() {
		if !started {
			// Clean up leaked background process and state
			cleanupCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			c.RepositoryServerStop(cleanupCtx, repoName)
		}
	}()

	// Poll for server startup
	ticker := time.NewTicker(500 * time.Millisecond)
	defer ticker.Stop()
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()

	for {
		select {
		case <-timer.C:
			return fmt.Errorf("timed out waiting for repository server to start")
		case <-serverDone:
			c.mu.Lock()
			err := c.serverErr
			c.mu.Unlock()
			if err != nil {
				return err
			}
			return fmt.Errorf("repository server exited unexpectedly during startup")
		case <-ticker.C:
			b, err := os.ReadFile(portPath)
			if err == nil && len(b) > 0 {
				parsedPort, err := strconv.Atoi(strings.TrimSpace(string(b)))
				if err == nil && parsedPort > 0 {
					if port != 0 && parsedPort != port {
						log.Printf("repository server port %d does not yet match expected %d, waiting...", parsedPort, port)
						continue
					}

					c.mu.Lock()
					c.repoPort = parsedPort
					c.mu.Unlock()

					running, err := c.IsPackageServerRunning(ctx, repoName)
					if err != nil {
						log.Printf("failed to check repository server status: %v", err)
						continue
					}
					if running {
						started = true
						return nil
					}
				}
			}
		case <-ctx.Done():
			return ctx.Err()
		}
	}
}

// RepositoryServerStop stops the repository server.
// repoName is unused in strict mode because FFXStrictClient only manages a single
// foreground repository server, but it is kept to satisfy the FFXClient interface.
func (c *FFXStrictClient) RepositoryServerStop(ctx context.Context, repoName string) error {
	c.mu.Lock()
	cancel := c.serverCancel
	doneChan := c.serverDone
	c.mu.Unlock()

	if cancel == nil {
		return nil
	}

	cancel()
	if doneChan != nil {
		select {
		case <-doneChan:
		case <-ctx.Done():
			return fmt.Errorf("timed out waiting for repository server to stop: %w", ctx.Err())
		}
	}

	return nil
}

func (c *FFXStrictClient) RepositoryServerList(ctx context.Context) (string, error) {
	out, err := c.ffxInst.RunAndGetOutput(ctx, "repository", "server", "list")
	if err != nil {
		return "", fmt.Errorf("repository server list failed: %w", err)
	}
	return out, nil
}

func (c *FFXStrictClient) IsPackageServerRunning(ctx context.Context, repoName string) (bool, error) {
	stdout, err := c.ffxInst.RunAndGetOutput(ctx, "--machine", "json", "repository", "server", "list")
	if err != nil {
		return false, fmt.Errorf("failed to list repository servers: %w", err)
	}

	var result struct {
		Ok struct {
			Data []struct {
				Name    string `json:"name"`
				Address string `json:"address"`
			} `json:"data"`
		} `json:"ok"`
		UserError struct {
			Message string `json:"message"`
		} `json:"user_error"`
		UnexpectedError struct {
			Message string `json:"message"`
		} `json:"unexpected_error"`
	}
	if err := json.Unmarshal([]byte(stdout), &result); err != nil {
		return false, fmt.Errorf("failed to unmarshal repository server list output %q: %w", stdout, err)
	}

	if result.UserError.Message != "" {
		return false, fmt.Errorf("repository server list user error: %s", result.UserError.Message)
	}
	if result.UnexpectedError.Message != "" {
		return false, fmt.Errorf("repository server list unexpected error: %s", result.UnexpectedError.Message)
	}

	c.mu.Lock()
	expectedPort := c.repoPort
	c.mu.Unlock()

	repoNamePrefix := fmt.Sprintf("%s.", repoName)
	for _, s := range result.Ok.Data {
		if s.Name == repoName || strings.HasPrefix(s.Name, repoNamePrefix) {
			port, err := parsePort(s.Address)
			if err != nil {
				return false, fmt.Errorf("failed to parse port from server address %q: %w", s.Address, err)
			}

			if expectedPort != 0 && port != expectedPort {
				continue
			}

			c.mu.Lock()
			c.repoPort = port
			c.mu.Unlock()
			return true, nil
		}
	}
	return false, nil
}

var xdgEnvVars = []string{"HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"}

func (c *FFXStrictClient) ApplyEnv(env []string) ([]string, error) {
	c.mu.Lock()
	target := c.ffxInst.GetTarget()
	c.mu.Unlock()

	// Make a copy to avoid modifying the caller's slice.
	newEnv := make([]string, 0, len(env)+len(xdgEnvVars)+4)

	// Filter out variables we are about to override to prevent duplicates.
	for _, val := range env {
		if strings.HasPrefix(val, "FUCHSIA_DEVICE_ADDR=") {
			continue
		}
		if target != "" && strings.HasPrefix(val, "FUCHSIA_NODENAME=") {
			continue
		}
		if isXdgVar(val) {
			continue
		}
		if strings.HasPrefix(val, "FFX_ISOLATE_DIR=") || strings.HasPrefix(val, "FUCHSIA_ANALYTICS_DISABLED=") {
			continue
		}
		newEnv = append(newEnv, val)
	}

	// Apply legacy isolation variables
	newEnv = append(newEnv, "FFX_ISOLATE_DIR="+c.ffxStrictDir, "FUCHSIA_ANALYTICS_DISABLED=1")

	// Apply isolation home directory overrides.
	for _, xdgVar := range xdgEnvVars {
		newEnv = append(newEnv, xdgVar+"="+c.ffxStrictDir)
	}

	// Add openssh-portable to PATH.
	newEnv = utils.PrependPath(newEnv, c.sshDir)

	if target != "" {
		newEnv = append(newEnv, "FUCHSIA_NODENAME="+target)
	}
	return newEnv, nil
}

func isXdgVar(val string) bool {
	for _, xdgVar := range xdgEnvVars {
		if strings.HasPrefix(val, xdgVar+"=") {
			return true
		}
	}
	return false
}

// SetupFfx is a no-op for strict mode since ffxutil.FFXInstance handles repository configuration internally.
func (c *FFXStrictClient) SetupFfx(ctx context.Context, repoName string) error {
	return nil
}

// DaemonStop is a no-op for strict mode because it operates daemonlessly.
func (c *FFXStrictClient) DaemonStop(ctx context.Context) error {
	return nil
}
