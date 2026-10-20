// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package cli

import (
	"context"
	"flag"
	"fmt"
	"strings"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/artifacts"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/device"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/ffx"
)

type repeatableBuildKind int

const (
	builderNameKind repeatableBuildKind = iota
	buildIdKind
	fuchsiaBuildDirKind
	productBundleDirKind
)

func (k repeatableBuildKind) String() string {
	switch k {
	case builderNameKind:
		return "builder-name"
	case buildIdKind:
		return "build-id"
	case fuchsiaBuildDirKind:
		return "fuchsia-build-dir"
	case productBundleDirKind:
		return "product-bundle-dir"
	default:
		return fmt.Sprintf("repeatableBuildKind(%d)", k)
	}
}

type RepeatableBuildConfig struct {
	archiveConfig     *ArchiveConfig
	deviceConfig      *DeviceConfig
	defaultBuildID    string
	blobFetchMode     artifacts.BlobFetchMode
	productBundleName string
	builds            []repeatableBuild
}

func NewRepeatableBuildConfig(
	fs *flag.FlagSet,
	archiveConfig *ArchiveConfig,
	deviceConfig *DeviceConfig,
	defaultBuildID string,
	prefix string,
) *RepeatableBuildConfig {
	c := &RepeatableBuildConfig{
		archiveConfig:  archiveConfig,
		deviceConfig:   deviceConfig,
		defaultBuildID: defaultBuildID,
		blobFetchMode:  artifacts.Unspecified,
	}

	fs.Var(
		repeatableBuildVar{c: c, kind: builderNameKind},
		fmt.Sprintf("%sbuilder-name", prefix),
		"Pave to the latest version of this builder",
	)
	fs.Var(
		repeatableBuildVar{c: c, kind: buildIdKind},
		fmt.Sprintf("%sbuild-id", prefix),
		"Pave to this specific build id, optionally with :<version_policy> (e.g. <build_id>:fromApiLevel)",
	)
	fs.Var(
		repeatableBuildVar{c: c, kind: fuchsiaBuildDirKind},
		fmt.Sprintf("%sfuchsia-build-dir", prefix),
		"Pave to the build in this fuchsia build output directory",
	)
	fs.Var(
		repeatableBuildVar{c: c, kind: productBundleDirKind},
		fmt.Sprintf("%sproduct-bundle-dir", prefix),
		"Update to the latest version of this builder",
	)
	fs.Var(
		&c.blobFetchMode,
		fmt.Sprintf("%sblob-fetch-mode", prefix),
		"Default blob fetch mode for builds ('prefetch' or 'lazy', default is 'unspecified')",
	)
	fs.StringVar(
		&c.productBundleName,
		fmt.Sprintf("%sproduct-bundle-name", prefix),
		"",
		"Name of the product bundle to use",
	)

	return c
}

func (c *RepeatableBuildConfig) Validate() error {
	if len(c.builds) == 0 && c.defaultBuildID == "" {
		return fmt.Errorf("no builds configured")
	}
	return nil
}

func (c *RepeatableBuildConfig) GetBuilds(
	ctx context.Context,
	deviceClient *device.Client,
	outputDir string,
) ([]artifacts.BuildWithVersion, error) {
	var builds []artifacts.BuildWithVersion
	for _, b := range c.builds {
		var build artifacts.Build
		switch b.kind {
		case builderNameKind:
			builder := c.archiveConfig.BuildArchive().GetBuilder(b.value)
			artifactsBuild, err := builder.GetLatestBuild(ctx, outputDir)
			if err != nil {
				return nil, fmt.Errorf("failed to lookup build id: %w", err)
			}
			build = artifactsBuild
		case buildIdKind:
			artifactsBuild, err := c.archiveConfig.BuildArchive().GetBuildByID(
				ctx,
				b.value,
				outputDir,
			)
			if err != nil {
				return nil, err
			}
			build = artifactsBuild
		case fuchsiaBuildDirKind:
			build = artifacts.NewFuchsiaDirBuild(b.value)
		case productBundleDirKind:
			build = artifacts.NewProductBundleDirBuild(b.value)
		}
		productBundle, err := build.GetProductBundle(ctx, c.productBundleName)
		if err != nil {
			return nil, err
		}
		builds = append(builds, artifacts.BuildWithVersion{
			ProductBundle: productBundle,
			Version:       ffx.FfxVersionPolicy(b.versionPolicy),
			BlobFetchMode: c.blobFetchMode,
		})
	}

	// Append the last build id as our final upgrade.
	if c.defaultBuildID != "" {
		build, err := c.archiveConfig.BuildArchive().GetBuildByID(
			ctx,
			c.defaultBuildID,
			outputDir,
		)
		if err != nil {
			return nil, err
		}
		productBundle, err := build.GetProductBundle(ctx, c.productBundleName)
		if err != nil {
			return nil, err
		}

		builds = append(builds, artifacts.BuildWithVersion{
			ProductBundle: productBundle,
			Version:       ffx.FfxVersionPolicyLatest,
			BlobFetchMode: c.blobFetchMode,
		})
	}

	return builds, nil
}

type repeatableBuild struct {
	kind          repeatableBuildKind
	value         string
	versionPolicy string
}

type repeatableBuildVar struct {
	c    *RepeatableBuildConfig
	kind repeatableBuildKind
}

func (v repeatableBuildVar) String() string {
	if v.c != nil {
		return fmt.Sprintf("%+v", v.c.builds)
	}
	return ""
}

func (v repeatableBuildVar) Set(s string) error {
	if s == "" {
		return fmt.Errorf("%s value cannot be empty", v.kind)
	}

	val := s
	versionPolicy := string(ffx.FfxVersionPolicyLatest)
	if v.kind == buildIdKind {
		// For build IDs, we allow the format "build_id:version_policy" to specify a specific
		// ffx version policy to use with this build.
		parts := strings.Split(s, ":")
		val = parts[0]
		if val == "" {
			return fmt.Errorf("build ID cannot be empty")
		}
		if len(parts) > 2 {
			return fmt.Errorf("too many parts in build ID specifier: %q", s)
		}
		if len(parts) > 1 {
			versionPolicy = parts[1]
			if versionPolicy != string(ffx.FfxVersionPolicyLatest) && versionPolicy != string(ffx.FfxVersionPolicyFromApiLevel) {
				return fmt.Errorf("invalid ffx version policy %q, must be 'latest' or 'fromApiLevel'", versionPolicy)
			}
		}
	}

	v.c.builds = append(v.c.builds, repeatableBuild{kind: v.kind, value: val, versionPolicy: versionPolicy})
	return nil
}
