// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffx

import (
	"bytes"
	"cmp"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

type IsolateDir struct {
	path string
}

func NewIsolateDir(path string) IsolateDir {
	return IsolateDir{path: path}
}

type FFXTool struct {
	ffxToolPath         string
	isolateDir          IsolateDir
	supportsPackageBlob *bool
}

func NewFFXTool(ffxToolPath string, isolateDir IsolateDir) (*FFXTool, error) {
	if _, err := os.Stat(ffxToolPath); err != nil {
		return nil, fmt.Errorf("error accessing %v: %w", ffxToolPath, err)
	}

	return &FFXTool{
		ffxToolPath:         ffxToolPath,
		isolateDir:          isolateDir,
		supportsPackageBlob: nil,
	}, nil
}

func (f *FFXTool) IsolateDir() IsolateDir {
	return f.isolateDir
}

func (f *FFXTool) StopDaemon(ctx context.Context) error {
	// TODO(https://fxbug.dev/415899721): We put a time
	// limit because the command fails when run inside an nsjail.
	// Remove when bug is fixed.
	args := []string{"daemon", "stop", "-t", "4000"}
	_, err := f.runFFXCmd(ctx, args...)
	return err
}

// TargetAddress represents a single address for a target.
// It corresponds to ffx's enum JsonTargetAddress.
type TargetAddress struct {
	Type    string `json:"type"`
	IP      string `json:"ip,omitempty"`       // for Type == "Ip"
	SSHPort uint16 `json:"ssh_port,omitempty"` // for Type == "Ip"
	CID     uint32 `json:"cid,omitempty"`      // for Type == "VSock" or "Usb"
}

// Target represents a single Fuchsia device/emulator.
// It corresponds to the Rust struct JsonTarget.
type TargetEntry struct {
	NodeName    string          `json:"nodename"`
	RCSState    string          `json:"rcs_state"` // "Y" or "N"
	Serial      string          `json:"serial"`
	TargetType  string          `json:"target_type"`  // board/product, like "core.x64" or "Unknown"
	TargetState string          `json:"target_state"` // e.g., "Product", "Fastboot", "Zedboot"
	Addresses   []TargetAddress `json:"addresses"`
	IsDefault   bool            `json:"is_default"`
	IsManual    bool            `json:"is_manual"`
}

func (f *FFXTool) TargetList(ctx context.Context) ([]TargetEntry, error) {
	args := []string{
		"--direct",
		"--machine",
		"json",
		"target",
		"list",
	}

	stdout, err := f.runFFXCmd(ctx, args...)
	if err != nil {
		return []TargetEntry{}, fmt.Errorf("ffx target list failed: %w", err)
	}

	if len(stdout) == 0 {
		return []TargetEntry{}, nil
	}

	var entries []TargetEntry
	if err := json.Unmarshal(stdout, &entries); err != nil {
		return []TargetEntry{}, err
	}

	return entries, nil
}

// GetDisambiguatedTarget is like TargetList, but returns exactly one target, enforcing the
// following rules:
// 1. Return the target if only one is found.
// 2. Return the default target if it is set.
// 3. Return the first target in the list if multiple targets are found, sorted by target name.
func (f *FFXTool) GetDisambiguatedTarget(ctx context.Context) (TargetEntry, error) {
	targets, err := f.TargetList(ctx)
	if err != nil {
		return TargetEntry{}, err
	}

	if len(targets) == 1 {
		return targets[0], nil
	}

	for _, v := range targets {
		if v.IsDefault {
			return v, nil
		}
	}

	return slices.MinFunc(targets, func(a, b TargetEntry) int {
		return cmp.Compare(a.NodeName, b.NodeName)
	}), nil
}

func (f *FFXTool) TargetListForNode(ctx context.Context, nodeName string) ([]TargetEntry, error) {
	entries, err := f.TargetList(ctx)
	if err != nil {
		return []TargetEntry{}, err
	}

	var matchingTargets []TargetEntry

	for _, target := range entries {
		if target.NodeName == nodeName {
			matchingTargets = append(matchingTargets, target)
		}
	}

	return matchingTargets, nil
}

func (f *FFXTool) WaitForTarget(ctx context.Context, address string) (TargetEntry, error) {
	for attempt := 0; attempt < 10; attempt++ {
		entries, err := f.TargetList(ctx)
		if err != nil {
			return TargetEntry{}, fmt.Errorf("failed to get target list: %w", err)
		}

		for _, target := range entries {
			for _, addr := range target.Addresses {
				if addr.Type == "Ip" && addr.IP == address {
					return target, nil
				}
			}
		}
		time.Sleep(5 * time.Second)
	}

	return TargetEntry{}, fmt.Errorf("no target found for address %v", address)
}

func (f *FFXTool) TargetGetSshAddress(ctx context.Context, target string) (string, error) {
	args := []string{
		"--direct",
		"--target",
		target,
		"target",
		"list",
		"--format",
		"addresses",
		"--no-probe",
		"--no-usb",
	}

	stdout, err := f.runFFXCmd(ctx, args...)
	if err != nil {
		return "", fmt.Errorf("ffx target list --format addresses failed: %w", err)
	}

	return strings.TrimSpace(string(stdout)), nil
}

func (f *FFXTool) SupportsZedbootDiscovery(ctx context.Context) (bool, error) {
	// Check if ffx is configured to resolve devices in zedboot.
	args := []string{
		"config",
		"get",
		"discovery.zedboot.enabled",
	}

	stdout, err := f.runFFXCmd(ctx, args...)
	if err != nil {
		// `ffx config get` exits with 2 if variable is undefined.
		if exiterr, ok := err.(*exec.ExitError); ok {
			if exiterr.ExitCode() == 2 {
				return false, nil
			}
		}

		return false, fmt.Errorf("ffx config get failed: %w", err)
	}

	// FIXME(https://fxbug.dev/42060660): Unfortunately we need to parse the raw string to see if it's true.
	if string(stdout) == "true\n" {
		return true, nil
	}

	return false, nil
}

func (f *FFXTool) TargetAdd(ctx context.Context, target string) error {
	args := []string{"target", "add", "--nowait", target}
	_, err := f.runFFXCmd(ctx, args...)
	return err
}

func (f *FFXTool) TargetGetSshTime(ctx context.Context, target string) (time.Duration, error) {
	args := []string{
		"--target",
		target,
		"target",
		"get-time",
	}

	t0 := time.Now()
	stdout, err := f.runFFXCmd(ctx, args...)
	t1 := time.Now()

	if err != nil {
		return 0, fmt.Errorf("ffx target get-time failed: %w", err)
	}

	t, err := strconv.Atoi(strings.TrimSpace(string(stdout)))
	if err != nil {
		return 0, fmt.Errorf("failed to parse ffx target-get-time output: %w", err)
	}

	// Estimate the latency as half the time to execute the command.
	latency := t1.Sub(t0) / 2

	// The output is in nanoseconds.
	monotonicTime := (time.Duration(t) * time.Nanosecond) - latency

	return monotonicTime, nil
}

func (f *FFXTool) TargetUpdateChannelSet(ctx context.Context, target string, channel string) error {
	args := []string{
		"--target",
		target,
		"target",
		"update",
		"channel",
		"set",
		channel,
	}

	_, err := f.runFFXCmd(ctx, args...)
	return err
}

func (f *FFXTool) TargetUpdateCheckNowMonitor(ctx context.Context, target string) ([]byte, error) {
	args := []string{
		"--target",
		target,
		"target",
		"update",
		"check-now",
		"--monitor",
	}

	return f.runFFXCmd(ctx, args...)
}

func (f *FFXTool) TargetUpdateForceInstallNoReboot(ctx context.Context, target string, url string) error {
	args := []string{
		"--direct",
		"--target",
		target,
		"target",
		"update",
		"force-install",
		url,
		"--reboot",
		"false",
	}

	_, err := f.runFFXCmd(ctx, args...)
	return err
}
func (f *FFXTool) Flasher() *Flasher {
	return newFlasher(f)
}

func (f *FFXTool) runFFXCmd(ctx context.Context, args ...string) ([]byte, error) {
	path, err := exec.LookPath(f.ffxToolPath)
	if err != nil {
		return []byte{}, err
	}

	// prepend a config flag for finding subtools that are compiled separately
	// in the same directory as ffx itself.
	args = append(
		[]string{
			"--log-level", "trace",
			"--isolate-dir", f.isolateDir.path,
			"--config", fmt.Sprintf("ffx.subtool-search-paths=%s", filepath.Dir(path)),
		},
		args...,
	)

	logger.Infof(ctx, "running: %s %q", path, args)
	cmd := exec.CommandContext(ctx, path, args...)
	var stdoutBuf bytes.Buffer
	cmd.Stdout = &stdoutBuf
	cmd.Stderr = os.Stderr

	cmdRet := cmd.Run()

	stdout := stdoutBuf.Bytes()
	if len(stdout) != 0 {
		logger.Infof(ctx, "%s", string(stdout))
	}

	if cmdRet == nil {
		logger.Infof(ctx, "finished running %s %q", path, args)
	} else {
		logger.Infof(ctx, "running %s %q failed with: %v", path, args, cmdRet)
	}
	return stdout, cmdRet
}

func (f *FFXTool) RepositoryCreate(ctx context.Context, repoDir, keysDir string) error {
	args := []string{
		"--config", "ffx_repository=true",
		"repository",
		"create",
		"--keys", keysDir,
		repoDir,
	}

	_, err := f.runFFXCmd(ctx, args...)
	return err
}

func (f *FFXTool) RepositoryPublish(ctx context.Context, repoDir string, packageManifests []string, additionalArgs ...string) error {
	args := []string{
		"repository",
		"publish",
	}

	for _, manifest := range packageManifests {
		args = append(args, "--package", manifest)
	}

	args = append(args, additionalArgs...)
	args = append(args, repoDir)

	_, err := f.runFFXCmd(ctx, args...)
	return err
}

func (f *FFXTool) SupportsPackageBlob(ctx context.Context) bool {
	if f.supportsPackageBlob == nil {
		_, err := f.runFFXCmd(ctx, "package", "blob", "--help")
		supportsPackageBlob := err == nil
		f.supportsPackageBlob = &supportsPackageBlob
	}
	return *f.supportsPackageBlob
}

func (f *FFXTool) DecompressBlobs(ctx context.Context, delivery_blobs []string, out_dir string) error {
	args := []string{
		"package",
		"blob",
		"decompress",
		"--output", out_dir,
	}

	args = append(args, delivery_blobs...)

	_, err := f.runFFXCmd(ctx, args...)
	return err
}
