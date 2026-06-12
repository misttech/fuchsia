// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package orchestrate

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	ffx "go.fuchsia.dev/fuchsia/tools/orchestrate/ffx"
	utils "go.fuchsia.dev/fuchsia/tools/orchestrate/utils"
)

// FFXStrictClient adapts ffxutil.FFXInstance to implement FFXClient in strict mode.
type FFXStrictClient struct {
	// TODO(jcecil): Remove this embedding once strict mode is fully implemented.
	*ffx.Ffx // Embed legacy client to fallback on unimplemented methods

	mu           sync.Mutex // synchronizes calls to ffxInst target state
	ffxInst      *ffxutil.FFXInstance
	outputsDir   string
	ffxStrictDir string
	sshInfo      *ffxutil.SSHInfo
	sshDir       string
	isTemp       bool
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
