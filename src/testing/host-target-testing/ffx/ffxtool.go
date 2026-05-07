// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package ffx

import (
	"context"
	"time"

	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

// FfxVersionPolicy specifies whether to use the latest ffx or infer the version from the target's API level.
type FfxVersionPolicy string

const (
	FfxVersionPolicyLatest       FfxVersionPolicy = "latest"
	FfxVersionPolicyFromApiLevel FfxVersionPolicy = "fromApiLevel"
)

// RunDir represents the execution directory for ffx.
type RunDir struct {
	path    string
	privKey string
}

func NewRunDir(path string) RunDir {
	return RunDir{path: path}
}

func NewRunDirWithPrivKey(path string, privKey string) RunDir {
	return RunDir{path: path, privKey: privKey}
}

func (d RunDir) PrivKey() string {
	return d.privKey
}

// TargetAddress represents a single address for a target.
// It corresponds to ffx's enum JsonTargetAddress.
type TargetAddress struct {
	Type    string `json:"type"`
	IP      string `json:"ip,omitempty"`       // for Type == "Ip"
	SSHPort uint16 `json:"ssh_port,omitempty"` // for Type == "Ip"
	CID     uint32 `json:"cid,omitempty"`      // for Type == "VSock" or "Usb"
}

// TargetEntry represents a single Fuchsia device/emulator.
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

// FFXToolImpl is the interface that abstracts the operations performed by the ffx tool implementations.
type FFXToolImpl interface {
	runFFXCmd(ctx context.Context, args ...string) ([]byte, error)
	TargetList(ctx context.Context) ([]TargetEntry, error)
	GetDisambiguatedTarget(ctx context.Context) (TargetEntry, error)
	TargetListForNode(ctx context.Context, nodeName string) ([]TargetEntry, error)
	WaitForTarget(ctx context.Context, address string) (TargetEntry, error)
	TargetGetSshAddress(ctx context.Context, target string) (string, error)
	SupportsZedbootDiscovery(ctx context.Context) (bool, error)
	TargetAdd(ctx context.Context, target string) error
	TargetGetSshTime(ctx context.Context, target string) (time.Duration, error)
	TargetUpdateChannelSet(ctx context.Context, target string, channel string) error
	TargetUpdateCheckNowMonitor(ctx context.Context, target string) ([]byte, error)
	TargetUpdateForceInstallNoReboot(ctx context.Context, target string, url string) error
	Flasher() *Flasher
	RepositoryCreate(ctx context.Context, repoDir, keysDir string) error
	RepositoryPublish(ctx context.Context, repoDir string, packageManifests []string, additionalArgs ...string) error
	SupportsPackageBlob(ctx context.Context) bool
	DecompressBlobs(ctx context.Context, deliveryBlobs []string, outDir string) error
	RegisterPackageRepository(ctx context.Context, repoURL string) error
	TargetGetLastRebootReason(ctx context.Context, target string) (string, error)
	RunDir() RunDir
	Run(ctx context.Context, args ...string) error
	RunAndGetOutput(ctx context.Context, args ...string) (string, error)
	StopDaemon(ctx context.Context) error
	ClearRunDir()
}

var _ FFXToolImpl = (*FFXTool)(nil)

// FFXTool is a concrete object that contains the implementation.
type FFXTool struct {
	impl FFXToolImpl
}

func NewFFXToolForVersion(ctx context.Context, ffxPath string, runDir RunDir, versionPolicy FfxVersionPolicy) (*FFXTool, error) {
	logger.Infof(ctx, "NewFFXToolForVersion called with version policy: %q", versionPolicy)
	// Note: The version policy is currently only used for logging in this CL.
	// It will be used in a follow-up CL to switch between daemon and strict mode.
	impl, err := newFfxDaemon(ctx, ffxPath, runDir)
	if err != nil {
		return nil, err
	}
	return &FFXTool{impl: impl}, nil
}

func NewFFXTool(ffxPath string, runDir RunDir) (*FFXTool, error) {
	return NewFFXToolForVersion(context.Background(), ffxPath, runDir, FfxVersionPolicyLatest)
}

func (t *FFXTool) runFFXCmd(ctx context.Context, args ...string) ([]byte, error) {
	return t.impl.runFFXCmd(ctx, args...)
}

func (t *FFXTool) TargetList(ctx context.Context) ([]TargetEntry, error) {
	return t.impl.TargetList(ctx)
}

// GetDisambiguatedTarget is like TargetList, but returns exactly one target, enforcing the
// following rules:
// 1. Return the target if only one is found.
// 2. Return the default target if it is set.
// 3. Return the first target in the list if multiple targets are found, sorted by target name.
func (t *FFXTool) GetDisambiguatedTarget(ctx context.Context) (TargetEntry, error) {
	return t.impl.GetDisambiguatedTarget(ctx)
}

func (t *FFXTool) TargetListForNode(ctx context.Context, nodeName string) ([]TargetEntry, error) {
	return t.impl.TargetListForNode(ctx, nodeName)
}

func (t *FFXTool) WaitForTarget(ctx context.Context, address string) (TargetEntry, error) {
	return t.impl.WaitForTarget(ctx, address)
}

func (t *FFXTool) TargetGetSshAddress(ctx context.Context, target string) (string, error) {
	return t.impl.TargetGetSshAddress(ctx, target)
}

func (t *FFXTool) SupportsZedbootDiscovery(ctx context.Context) (bool, error) {
	return t.impl.SupportsZedbootDiscovery(ctx)
}

func (t *FFXTool) TargetAdd(ctx context.Context, target string) error {
	return t.impl.TargetAdd(ctx, target)
}

func (t *FFXTool) TargetGetSshTime(ctx context.Context, target string) (time.Duration, error) {
	return t.impl.TargetGetSshTime(ctx, target)
}

func (t *FFXTool) TargetUpdateChannelSet(ctx context.Context, target string, channel string) error {
	return t.impl.TargetUpdateChannelSet(ctx, target, channel)
}

func (t *FFXTool) TargetUpdateCheckNowMonitor(ctx context.Context, target string) ([]byte, error) {
	return t.impl.TargetUpdateCheckNowMonitor(ctx, target)
}

func (t *FFXTool) TargetUpdateForceInstallNoReboot(ctx context.Context, target string, url string) error {
	return t.impl.TargetUpdateForceInstallNoReboot(ctx, target, url)
}

func (t *FFXTool) Flasher() *Flasher {
	return t.impl.Flasher()
}

func (t *FFXTool) RepositoryCreate(ctx context.Context, repoDir, keysDir string) error {
	return t.impl.RepositoryCreate(ctx, repoDir, keysDir)
}

func (t *FFXTool) RepositoryPublish(ctx context.Context, repoDir string, packageManifests []string, additionalArgs ...string) error {
	return t.impl.RepositoryPublish(ctx, repoDir, packageManifests, additionalArgs...)
}

func (t *FFXTool) SupportsPackageBlob(ctx context.Context) bool {
	return t.impl.SupportsPackageBlob(ctx)
}

func (t *FFXTool) DecompressBlobs(ctx context.Context, delivery_blobs []string, out_dir string) error {
	return t.impl.DecompressBlobs(ctx, delivery_blobs, out_dir)
}

func (t *FFXTool) RegisterPackageRepository(ctx context.Context, repo_url string) error {
	return t.impl.RegisterPackageRepository(ctx, repo_url)
}

func (t *FFXTool) TargetGetLastRebootReason(ctx context.Context, target string) (string, error) {
	return t.impl.TargetGetLastRebootReason(ctx, target)
}

func (t *FFXTool) RunDir() RunDir {
	return t.impl.RunDir()
}

func (t *FFXTool) Run(ctx context.Context, args ...string) error {
	return t.impl.Run(ctx, args...)
}

func (t *FFXTool) RunAndGetOutput(ctx context.Context, args ...string) (string, error) {
	return t.impl.RunAndGetOutput(ctx, args...)
}

func (t *FFXTool) StopDaemon(ctx context.Context) error {
	return t.impl.StopDaemon(ctx)
}

func (t *FFXTool) ClearRunDir() {
	t.impl.ClearRunDir()
}
