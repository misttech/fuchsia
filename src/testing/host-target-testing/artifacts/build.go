// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package artifacts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"
	"sync"

	"golang.org/x/crypto/ssh"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/avb"
	pmBuild "go.fuchsia.dev/fuchsia/src/testing/host-target-testing/build"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/ffx"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/omaha_tool"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/packages"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/paver"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/util"
	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/zbi"
	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
)

type BlobFetchMode int

const (
	// Unspecified indicates that no blob fetch mode was explicitly specified.
	Unspecified BlobFetchMode = iota

	// PrefetchBlobs will download all the blobs from a build when `GetPackageRepository()` is called.
	PrefetchBlobs

	// LazilyFetchBlobs will only download blobs when they are accessed.
	LazilyFetchBlobs
)

func (m BlobFetchMode) String() string {
	switch m {
	case Unspecified:
		return "unspecified"
	case PrefetchBlobs:
		return "prefetch"
	case LazilyFetchBlobs:
		return "lazy"
	default:
		return fmt.Sprintf("BlobFetchMode(%d)", m)
	}
}

func (m *BlobFetchMode) Set(s string) error {
	mode, err := ParseBlobFetchMode(s)
	if err != nil {
		return err
	}
	*m = mode
	return nil
}

func ParseBlobFetchMode(s string) (BlobFetchMode, error) {
	switch strings.ToLower(s) {
	case "unspecified":
		return Unspecified, nil
	case "prefetch":
		return PrefetchBlobs, nil
	case "lazy":
		return LazilyFetchBlobs, nil
	default:
		return Unspecified, fmt.Errorf("invalid blob fetch mode %q, must be 'prefetch' or 'lazy'", s)
	}
}

const (
	// Product Bundle manifest which is used to locate VBmeta
	ProductBundleManifest = "product_bundle.json"

	relativeBootserverPath = "tools/linux-x64/bootserver"

	relativeFfxPath = "tools/linux-x64/ffx"

	relativeFlashManifest = "images/flash.json"

	productBundleBuildInfoPath = "build_api/build_info.json"
)

type ProductBundle interface {
	// String returns a string identifier for this product bundle.
	String() string

	// Build returns the Build associated with this product bundle.
	Build() Build

	// GetProductBundleDir returns the path to the product bundle.
	GetProductBundleDir(ctx context.Context) (string, error)

	// GetPackageRepository returns a Repository for this product bundle.
	GetPackageRepository(
		ctx context.Context,
		blobFetchMode BlobFetchMode,
		ffxRunDir ffx.RunDir,
		version ffx.FfxVersionPolicy,
		hostFfx *ffx.FFXTool,
	) (*packages.Repository, error)

	// GetVbmetaPath downloads and returns a path to the zircon-a vbmeta image.
	GetVbmetaPath(ctx context.Context) (string, error)
}

type DirProductBundle struct {
	dir string
}

func NewDirProductBundle(dir string) *DirProductBundle {
	return &DirProductBundle{
		dir: dir,
	}
}

func (pb *DirProductBundle) Dir() string {
	return pb.dir
}

func (pb *DirProductBundle) RepositoryInfo() (string, *int, error) {
	return util.ParseProductBundleRepository(pb.dir)
}

func (pb *DirProductBundle) GetPackageRepository(
	ctx context.Context,
	ffxTool *ffx.FFXTool,
) (*packages.Repository, error) {
	blobsDir, blobType, err := pb.RepositoryInfo()
	if err != nil {
		return nil, err
	}
	blobFS := packages.NewDirBlobStore(blobsDir)
	return packages.NewRepository(ctx, pb.dir, blobFS, ffxTool, blobType)
}

func (pb *DirProductBundle) GetVbmetaPath() (string, error) {
	productBundle, err := util.ParseProductBundle(pb.dir)
	if err != nil {
		return "", err
	}
	return productBundle.GetSystemAImage("vbmeta", "zircon-a")
}

func (pb *DirProductBundle) TransferManifestBlobs() ([]string, error) {
	transferManifestPath := filepath.Join(pb.dir, "transfer.json")
	return util.ParseTransferManifestBlobs(transferManifestPath)
}

type Build interface {
	// String returns a string identifier for this build.
	String() string

	// OutputDir is a location where we can write generated test files.
	OutputDir() string

	// GetBootserver returns the path to the bootserver used for paving.
	GetBootserver(ctx context.Context) (string, error)

	// GetFfx returns the FFXTool from this build.
	GetFfx(
		ctx context.Context,
		ffxRunDir ffx.RunDir,
		version ffx.FfxVersionPolicy,
	) (*ffx.FFXTool, error)

	// GetFlashManifest returns the path to the flash manifest used for flashing.
	GetFlashManifest(ctx context.Context) (string, error)

	// GetProductBundle returns a ProductBundle for this build.
	GetProductBundle(
		ctx context.Context,
		productBundleName string,
	) (ProductBundle, error)

	// GetPaverDir downloads and returns the directory containing the images
	// and image manifest.
	GetPaverDir(ctx context.Context) (string, error)

	// GetPaver downloads and returns a paver for the build.
	GetPaver(
		ctx context.Context,
		sshPublicKey ssh.PublicKey,
	) (paver.Paver, error)
}

type BuildWithVersion struct {
	ProductBundle ProductBundle
	Version       ffx.FfxVersionPolicy
	BlobFetchMode BlobFetchMode
}

// ArtifactsBuild represents the build artifacts for a specific build.
type ArtifactsBuild struct {
	id                   string
	archive              *Archive
	blobsDir             string
	buildDir             string
	buildImageDir        string
	productBundles       *util.ProductBundles
	cachedProductBundles map[string]*ArtifactsProductBundle
	srcs                 map[string]struct{}
}

func (b *ArtifactsBuild) OutputDir() string {
	return b.buildDir
}

func (b *ArtifactsBuild) GetBootserver(ctx context.Context) (string, error) {
	// We need build images to flash, so lets download them.
	_, err := b.GetBuildImages(ctx)
	if err != nil {
		return "", err
	}

	// Use the latest bootserver if possible because the one uploaded with the artifacts may not include bug fixes.
	currentBuildId := os.Getenv("BUILDBUCKET_ID")
	if currentBuildId == "" {
		currentBuildId = b.id
	}

	bootserverPath := filepath.Join(b.buildDir, relativeBootserverPath)
	if err := b.archive.download(
		ctx,
		currentBuildId,
		false,
		b.buildDir,
		[]string{relativeBootserverPath},
	); err != nil {
		return "", fmt.Errorf("failed to download bootserver: %w", err)
	}

	// Make bootserver executable.
	if err := os.Chmod(bootserverPath, os.ModePerm); err != nil {
		return "", fmt.Errorf("failed to make bootserver executable: %w", err)
	}

	return bootserverPath, nil
}

func (b *ArtifactsBuild) GetFfx(
	ctx context.Context,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
) (*ffx.FFXTool, error) {
	// Get the ffx tool for this build.
	ffxPath := filepath.Join(b.buildDir, relativeFfxPath)

	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		[]string{relativeFfxPath},
	); err != nil {
		return nil, fmt.Errorf("failed to download ffxPath: %w", err)
	}

	for _, subtool := range []string{"package", "product", "component", "emulator", "repository", "product-bundle"} {
		if err := b.downloadSubtool(ctx, subtool); err != nil {
			return nil, err
		}
	}

	// Make ffx executable.
	if err := os.Chmod(ffxPath, os.ModePerm); err != nil {
		return nil, fmt.Errorf("failed to make ffxPath executable: %w", err)
	}

	return ffx.NewFFXToolForVersion(ctx, ffxPath, ffxRunDir, version, "")
}

func (b *ArtifactsBuild) downloadSubtool(
	ctx context.Context,
	name string,
) error {
	ffxPath := filepath.Join(b.buildDir, relativeFfxPath)
	subtoolPath := ffxPath + "-" + name

	// Ignore error, not all builds upload all subtools
	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		[]string{relativeFfxPath + "-" + name},
	); err == nil {
		if err := os.Chmod(subtoolPath, os.ModePerm); err != nil {
			return fmt.Errorf("failed to make subtool %s executable: %w", name, err)
		}
		// Try to download the manifest, but ignore error if it fails
		_ = b.archive.download(
			ctx,
			b.id,
			false,
			b.buildDir,
			[]string{relativeFfxPath + "-" + name + ".json"},
		)
	}
	return nil
}

func (b *ArtifactsBuild) GetFlashManifest(ctx context.Context) (string, error) {
	// We need build images to flash.
	_, err := b.GetBuildImages(ctx)
	if err != nil {
		return "", err
	}

	flashManifest := filepath.Join(b.buildDir, relativeFlashManifest)
	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		[]string{relativeFlashManifest},
	); err != nil {
		return "", fmt.Errorf("failed to download flash.json for flasher: %w", err)
	}

	return flashManifest, nil
}

func (b *ArtifactsBuild) getDefaultProductBundleName(ctx context.Context) (string, error) {
	if _, ok := b.srcs[productBundleBuildInfoPath]; !ok {
		return "", fmt.Errorf("%s not found in build %s", productBundleBuildInfoPath, b.id)
	}

	buildInfoPath := filepath.Join(b.buildDir, productBundleBuildInfoPath)
	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		[]string{productBundleBuildInfoPath},
	); err != nil {
		logger.Errorf(ctx, "failed to download build info for build %s: %v", b.id, err)
		return "", fmt.Errorf("failed to download build info for build %s: %w", b.id, err)
	}

	return util.ParseBuildInfoProductBundleName(buildInfoPath)
}

func (b *ArtifactsBuild) GetProductBundle(
	ctx context.Context,
	productBundleName string,
) (ProductBundle, error) {
	var pbPath string
	if b.productBundles.Count() > 0 || productBundleName != "" {
		lookupName := productBundleName
		if lookupName == "" && b.productBundles.Count() > 1 {
			if defaultName, err := b.getDefaultProductBundleName(ctx); err == nil && b.productBundles.Has(defaultName) {
				lookupName = defaultName
			}
		}

		var err error
		pbPath, err = b.productBundles.FindPath(lookupName)
		if err != nil {
			return nil, fmt.Errorf("failed to find product bundle for build %s: %w", b.id, err)
		}
	}
	if b.cachedProductBundles == nil {
		b.cachedProductBundles = make(map[string]*ArtifactsProductBundle)
	}
	key := productBundleName + ":" + pbPath
	if pb, ok := b.cachedProductBundles[key]; ok {
		return pb, nil
	}
	pb := &ArtifactsProductBundle{
		build:             b,
		productBundleName: productBundleName,
		pbPath:            pbPath,
	}
	b.cachedProductBundles[key] = pb
	return pb, nil
}

type ArtifactsProductBundle struct {
	build             *ArtifactsBuild
	productBundleName string
	pbPath            string
	productBundle     *DirProductBundle
	packages          *packages.Repository
}

func (pb *ArtifactsProductBundle) String() string {
	if pb.productBundleName != "" {
		return fmt.Sprintf("%s/%s", pb.build.id, pb.productBundleName)
	}
	return pb.build.id
}

func (pb *ArtifactsProductBundle) Build() Build {
	return pb.build
}

func (pb *ArtifactsProductBundle) getProductBundle(ctx context.Context) (*DirProductBundle, error) {
	if pb.productBundle != nil {
		return pb.productBundle, nil
	}
	if pb.pbPath == "" {
		return nil, fmt.Errorf("no product bundle found for build %s", pb.build.id)
	}

	logger.Infof(ctx, "downloading product bundle for build %s", pb.build.id)

	pbArtifacts := []string{}
	prefix := pb.pbPath + "/"
	for artifact := range pb.build.srcs {
		if strings.HasPrefix(artifact, prefix) {
			pbArtifacts = append(pbArtifacts, artifact)
		}
	}
	if len(pbArtifacts) == 0 {
		return nil, fmt.Errorf(
			"no product bundle artifacts found under %s for build %s",
			pb.pbPath,
			pb.build.id,
		)
	}

	productBundleDir := filepath.Join(pb.build.buildDir, pb.pbPath)
	if err := pb.build.archive.download(
		ctx,
		pb.build.id,
		false,
		pb.build.buildDir,
		pbArtifacts,
	); err != nil {
		logger.Errorf(ctx, "failed to download product bundle for build %s: %v", pb.build.id, err)
		return nil, fmt.Errorf("failed to download product bundle for build %s: %w", pb.build.id, err)
	}

	pb.productBundle = NewDirProductBundle(productBundleDir)
	return pb.productBundle, nil
}

func (pb *ArtifactsProductBundle) GetProductBundleDir(ctx context.Context) (string, error) {
	dirPb, err := pb.getProductBundle(ctx)
	if err != nil {
		return "", err
	}
	return dirPb.Dir(), nil
}

// GetPackageRepository returns a Repository for this product bundle. It tries
// to download a package when all the artifacts are stored in individual files,
// which is how modern builds publish their build artifacts.
func (pb *ArtifactsProductBundle) GetPackageRepository(
	ctx context.Context,
	fetchMode BlobFetchMode,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
	hostFfx *ffx.FFXTool,
) (*packages.Repository, error) {
	if pb.packages != nil {
		return pb.packages, nil
	}

	var (
		repoDir  string
		blobType *int
		err      error
	)
	_, hasAllBlobs := pb.build.srcs["packages/all_blobs.json"]
	if pb.pbPath != "" && (pb.productBundleName != "" || !hasAllBlobs) {
		repoDir, blobType, err = pb.downloadProductBundlePackageRepository(ctx, fetchMode)
	} else {
		repoDir, blobType, err = pb.build.downloadPackageRepository(ctx, fetchMode)
	}
	if err != nil {
		return nil, err
	}

	var ffxTool *ffx.FFXTool
	if version == ffx.FfxVersionPolicyLatest && hostFfx != nil {
		ffxTool = hostFfx
	} else {
		ffxTool, err = pb.build.GetFfx(ctx, ffxRunDir, version)
		if err != nil {
			return nil, fmt.Errorf("failed to get ffx: %w", err)
		}
	}

	p, err := packages.NewRepository(
		ctx,
		repoDir,
		&proxyBlobStore{
			b:        pb.build,
			ffx:      ffxTool,
			blobsDir: pb.build.blobsDir,
			blobType: blobType,
		},
		ffxTool,
		blobType,
	)
	if err != nil {
		return nil, err
	}
	pb.packages = p

	return pb.packages, nil
}

func (pb *ArtifactsProductBundle) downloadProductBundlePackageRepository(
	ctx context.Context,
	fetchMode BlobFetchMode,
) (string, *int, error) {
	logger.Infof(ctx, "downloading product bundle package repository from %s", pb.pbPath)

	pbJSON := path.Join(pb.pbPath, ProductBundleManifest)
	transferJSON := path.Join(pb.pbPath, "transfer.json")
	keysPrefix := path.Join(pb.pbPath, "keys") + "/"
	repoPrefix := path.Join(pb.pbPath, "repository") + "/"

	packageSrcs := []string{}
	for src := range pb.build.srcs {
		if src == pbJSON || src == transferJSON || strings.HasPrefix(src, keysPrefix) || strings.HasPrefix(src, repoPrefix) {
			packageSrcs = append(packageSrcs, src)
		}
	}
	if len(packageSrcs) == 0 {
		return "", nil, fmt.Errorf("no product bundle repository artifacts found under %s for build %s", pb.pbPath, pb.build.id)
	}

	pbDir := filepath.Join(pb.build.buildDir, pb.pbPath)

	if err := pb.build.archive.download(
		ctx,
		pb.build.id,
		false,
		pb.build.buildDir,
		packageSrcs,
	); err != nil {
		logger.Errorf(ctx, "failed to download product bundle packages for build %s to %s: %v", pb.build.id, pbDir, err)
		return "", nil, fmt.Errorf("failed to download product bundle packages for build %s to %s: %w", pb.build.id, pbDir, err)
	}

	dirPb := NewDirProductBundle(pbDir)
	_, blobType, err := dirPb.RepositoryInfo()
	if err != nil {
		return "", nil, err
	}

	if fetchMode == PrefetchBlobs {
		blobsList, err := dirPb.TransferManifestBlobs()
		if err != nil {
			return "", nil, fmt.Errorf("failed to parse transfer manifest in %s: %w", dirPb.Dir(), err)
		}
		logger.Infof(ctx, "transfer.json contains %d blobs", len(blobsList))
		if err := pb.build.archive.download(
			ctx,
			pb.build.id,
			true,
			filepath.Dir(pb.build.blobsDir),
			blobsList,
		); err != nil {
			logger.Errorf(ctx, "failed to download blobs to %s: %v", filepath.Dir(pb.build.blobsDir), err)
			return "", nil, fmt.Errorf("failed to download blobs to %s: %w", filepath.Dir(pb.build.blobsDir), err)
		}
	}

	return dirPb.Dir(), blobType, nil
}

func (pb *ArtifactsProductBundle) GetVbmetaPath(ctx context.Context) (string, error) {
	path, err := pb.getVbmetaPathFromProductBundle(ctx)
	if err == nil {
		return path, nil
	}

	logger.Warningf(ctx, "failed to get vbmeta from product bundle, trying images: %v", err)
	return pb.build.getVbmetaPathFromImages(ctx)
}

func (pb *ArtifactsProductBundle) getVbmetaPathFromProductBundle(ctx context.Context) (string, error) {
	dirPb, err := pb.getProductBundle(ctx)
	if err != nil {
		return "", err
	}

	return dirPb.GetVbmetaPath()
}

func (b *ArtifactsBuild) downloadPackageRepository(
	ctx context.Context,
	fetchMode BlobFetchMode,
) (string, *int, error) {
	logger.Infof(ctx, "downloading package repository")

	// Make sure the blob contains the `packages/all_blobs.json`.
	if _, ok := b.srcs["packages/all_blobs.json"]; !ok {
		logger.Errorf(ctx, "blobs manifest doesn't exist for build %s", b.id)
		return "", nil, fmt.Errorf("blob manifest doesn't exist for build %s", b.id)
	}

	packageSrcs := []string{}
	for src := range b.srcs {
		if strings.HasPrefix(src, "packages/") {
			packageSrcs = append(packageSrcs, src)
		}
	}

	packagesDir := filepath.Join(b.buildDir, "packages")
	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		packageSrcs,
	); err != nil {
		logger.Errorf(ctx, "failed to download packages for build %s to %s: %v", b.id, packagesDir, err)
		return "", nil, fmt.Errorf("failed to download packages for build %s to %s: %w", b.id, packagesDir, err)
	}

	blobsManifest := filepath.Join(packagesDir, "all_blobs.json")
	blobsData, err := os.ReadFile(blobsManifest)
	if err != nil {
		return "", nil, fmt.Errorf("failed to read blobs manifest: %w", err)
	}

	var blobs []build.Blob
	if err := json.Unmarshal(blobsData, &blobs); err != nil {
		return "", nil, fmt.Errorf("failed to unmarshal blobs JSON: %w", err)
	}

	deliveryBlobConfigPath := filepath.Join(packagesDir, "delivery_blob_config.json")
	relativeBlobsDir, err := build.GetBlobsDir(deliveryBlobConfigPath)
	if err != nil {
		return "", nil, fmt.Errorf("failed to get blobs dir: %w", err)
	}

	var blobsList []string
	for _, blob := range blobs {
		blobsList = append(blobsList, filepath.Join(relativeBlobsDir, blob.Merkle))
	}
	logger.Infof(ctx, "all_blobs contains %d blobs", len(blobs))

	if fetchMode == PrefetchBlobs {
		if err := b.archive.download(
			ctx,
			b.id,
			true,
			filepath.Join(b.blobsDir),
			blobsList,
		); err != nil {
			logger.Errorf(ctx, "failed to download blobs to %s: %v", b.blobsDir, err)
			return "", nil, fmt.Errorf("failed to download blobs to %s: %w", b.blobsDir, err)
		}
	}

	blobType, err := build.GetDeliveryBlobType(deliveryBlobConfigPath)
	if err != nil {
		return "", nil, fmt.Errorf("failed to get delivery blob type: %w", err)
	}

	return packagesDir, blobType, nil
}

type blobLock struct {
	mu   sync.Mutex
	refs int
}

type proxyBlobStore struct {
	b        *ArtifactsBuild
	ffx      *ffx.FFXTool
	blobsDir string
	blobType *int
	// mu protects locks so on-demand blob fetching and decompression in
	// BlobPath are synchronized per merkle root.
	mu    sync.Mutex
	locks map[pmBuild.MerkleRoot]*blobLock
}

// lockBlob grabs the blob lock, and returns a function that unlocks the blob.
func (fs *proxyBlobStore) lockBlob(merkle pmBuild.MerkleRoot) func() {
	fs.mu.Lock()
	if fs.locks == nil {
		fs.locks = make(map[pmBuild.MerkleRoot]*blobLock)
	}
	l, ok := fs.locks[merkle]
	if !ok {
		l = &blobLock{}
		fs.locks[merkle] = l
	}
	l.refs++
	fs.mu.Unlock()

	l.mu.Lock()
	return func() {
		l.mu.Unlock()
		fs.mu.Lock()
		l.refs--
		if l.refs == 0 {
			delete(fs.locks, merkle)
		}
		fs.mu.Unlock()
	}
}

func (fs *proxyBlobStore) PrefetchBlobs(
	ctx context.Context,
	deliveryBlobType *int,
	merkles []pmBuild.MerkleRoot,
) error {
	if len(merkles) == 0 {
		return nil
	}

	var relativeBlobsDir string

	if deliveryBlobType == nil {
		relativeBlobsDir = "blobs"
	} else {
		deliveryBlobType := strconv.Itoa(*deliveryBlobType)
		relativeBlobsDir = filepath.Join("blobs", deliveryBlobType)
	}

	srcs := []string{}
	for _, merkle := range merkles {
		src := filepath.Join(relativeBlobsDir, merkle.String())
		srcs = append(srcs, src)
	}

	// Start downloading the blobs. The package resolver will only fetch a blob
	// once, so we don't need to deduplicate requests on our side.

	return fs.b.archive.download(
		ctx,
		fs.b.id,
		true,
		filepath.Dir(fs.blobsDir),
		srcs,
	)
}

func (fs *proxyBlobStore) BlobPath(ctx context.Context, deliveryBlobType *int, merkle pmBuild.MerkleRoot) (string, error) {
	var path string
	if deliveryBlobType == nil {
		path = filepath.Join(fs.blobsDir, merkle.String())
	} else {
		path = filepath.Join(fs.blobsDir, strconv.Itoa(*deliveryBlobType), merkle.String())
	}

	// First, try to read the blob from the directory
	if _, err := os.Stat(path); err == nil {
		return path, nil
	}

	// The file didn't exist when we checked, so grab the per-blob lock.
	unlock := fs.lockBlob(merkle)
	defer unlock()

	// We could have raced with another thread that fetched or decompressed the blob, so see if it
	// exists now.
	if _, err := os.Stat(path); err == nil {
		return path, nil
	}

	// Decompress delivery blob if possible.
	if deliveryBlobType == nil && fs.blobType != nil && fs.ffx.SupportsPackageBlob(ctx) {
		deliveryBlobPath := filepath.Join(fs.blobsDir, strconv.Itoa(*fs.blobType), merkle.String())
		_, err := os.Stat(deliveryBlobPath)
		if err != nil {
			err = fs.PrefetchBlobs(ctx, fs.blobType, []pmBuild.MerkleRoot{merkle})
			if err != nil {
				logger.Errorf(ctx, "failed to prefetch type %d blob %s: %v", *fs.blobType, merkle, err)
			}
		}
		if err == nil {
			err = fs.ffx.DecompressBlobs(ctx, []string{deliveryBlobPath}, fs.blobsDir)
			if err == nil {
				return path, nil
			}
			logger.Errorf(ctx, "failed to decompress blob %s: %v", deliveryBlobPath, err)
		}
	}

	if err := fs.PrefetchBlobs(ctx, deliveryBlobType, []pmBuild.MerkleRoot{merkle}); err != nil {
		return "", err
	}

	return path, nil
}

func (fs *proxyBlobStore) OpenBlob(ctx context.Context, deliveryBlobType *int, merkle pmBuild.MerkleRoot) (*os.File, error) {
	path, err := fs.BlobPath(ctx, deliveryBlobType, merkle)
	if err != nil {
		return nil, err
	}

	return os.Open(path)
}

func (fs *proxyBlobStore) BlobSize(ctx context.Context, deliveryBlobType *int, merkle pmBuild.MerkleRoot) (uint64, error) {
	f, err := fs.OpenBlob(ctx, deliveryBlobType, merkle)
	if err != nil {
		return 0, err
	}
	defer f.Close()

	if s, err := f.Stat(); err == nil {
		if s.Size() < 0 {
			return 0, fmt.Errorf("merkle %s has size less than zero: %d", merkle, s.Size())
		}
		return uint64(s.Size()), nil
	} else {
		return 0, err
	}
}

func (fs *proxyBlobStore) Dir() string {
	return fs.blobsDir
}

// GetBuildImages downloads the build images for a specific build id.
// Returns a path to the directory of the downloaded images or an error if it
// fails to download.
func (b *ArtifactsBuild) GetBuildImages(ctx context.Context) (string, error) {
	if b.buildImageDir != "" {
		return b.buildImageDir, nil
	}

	logger.Infof(ctx, "downloading build images")

	imageDir := filepath.Join(b.buildDir, "images")
	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		[]string{path.Join("images", paver.ImageManifest)},
	); err != nil {
		return "", fmt.Errorf("failed to download image manifest: %w", err)
	}
	imagesJSON := filepath.Join(imageDir, paver.ImageManifest)
	f, err := os.Open(imagesJSON)
	if err != nil {
		return "", fmt.Errorf("failed to open %q: %w", imagesJSON, err)
	}
	defer f.Close()

	var items []build.Image
	if err := json.NewDecoder(f).Decode(&items); err != nil {
		return "", fmt.Errorf("failed to parse build image %q: %w", imagesJSON, err)
	}

	// Get list of all available images to download and only download
	// the ones needed for flashing or paving.
	imageSrcMap := make(map[string]struct{})
	for src := range b.srcs {
		if strings.HasPrefix(src, "images/") {
			imageSrcMap[src] = struct{}{}
		}
	}
	imageSrcs := []string{}
	for _, item := range items {
		src := path.Join("images", item.Path)
		if _, ok := imageSrcMap[src]; ok {
			imageSrcs = append(imageSrcs, src)
		}
	}

	if err := b.archive.download(
		ctx,
		b.id,
		false,
		b.buildDir,
		imageSrcs,
	); err != nil {
		return "", fmt.Errorf("failed to download images to %s: %w", imageDir, err)
	}

	b.buildImageDir = imageDir
	return b.buildImageDir, nil
}

func (b *ArtifactsBuild) GetPaverDir(ctx context.Context) (string, error) {
	return b.GetBuildImages(ctx)
}

// GetPaver downloads and returns a paver for the build.
func (b *ArtifactsBuild) GetPaver(
	ctx context.Context,
	sshPublicKey ssh.PublicKey,
) (paver.Paver, error) {
	return b.getPaver(ctx, sshPublicKey)
}

func (b *ArtifactsBuild) getPaver(
	ctx context.Context,
	sshPublicKey ssh.PublicKey,
) (*paver.BuildPaver, error) {
	buildImageDir, err := b.GetPaverDir(ctx)
	if err != nil {
		return nil, err
	}

	bootserverPath, err := b.GetBootserver(ctx)
	if err != nil {
		return nil, err
	}

	return paver.NewBuildPaver(bootserverPath, buildImageDir, paver.SSHPublicKey(sshPublicKey))
}

func (b *ArtifactsBuild) getVbmetaPathFromImages(ctx context.Context) (string, error) {
	buildImageDir, err := b.GetBuildImages(ctx)
	if err != nil {
		return "", err
	}
	imagesJSON := filepath.Join(buildImageDir, paver.ImageManifest)
	f, err := os.Open(imagesJSON)
	if err != nil {
		return "", fmt.Errorf("failed to open %q: %w", imagesJSON, err)
	}
	defer f.Close()

	var items []struct {
		Name string `json:"name"`
		Path string `json:"path"`
		Type string `json:"type"`
	}
	if err := json.NewDecoder(f).Decode(&items); err != nil {
		return "", fmt.Errorf("failed to parse vbmeta image %q: %w", imagesJSON, err)
	}

	for _, item := range items {
		if item.Name == "zircon-a" && item.Type == "vbmeta" {
			return filepath.Join(buildImageDir, item.Path), nil
		}
	}

	return "", fmt.Errorf("failed to file zircon-a vbmeta in %q", imagesJSON)
}

func (b *ArtifactsBuild) String() string {
	return b.id
}

type FuchsiaDirBuild struct {
	dir            string
	productBundles *util.ProductBundles
}

func NewFuchsiaDirBuild(dir string) *FuchsiaDirBuild {
	return &FuchsiaDirBuild{
		dir: dir,
	}
}

func (b *FuchsiaDirBuild) String() string {
	return b.dir
}

func (b *FuchsiaDirBuild) OutputDir() string {
	return b.dir
}

func (b *FuchsiaDirBuild) GetBootserver(ctx context.Context) (string, error) {
	return filepath.Join(b.dir, "host_x64/bootserver_new"), nil
}

func (b *FuchsiaDirBuild) GetFfx(
	ctx context.Context,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
) (*ffx.FFXTool, error) {
	ffxPath := filepath.Join(b.dir, "host_x64/ffx")
	return ffx.NewFFXToolForVersion(ctx, ffxPath, ffxRunDir, version, "")
}

func (b *FuchsiaDirBuild) GetFlashManifest(ctx context.Context) (string, error) {
	return filepath.Join(b.dir, "flash.json"), nil
}

func (b *FuchsiaDirBuild) getProductBundles() (*util.ProductBundles, error) {
	if b.productBundles != nil {
		return b.productBundles, nil
	}
	productBundlesPath := filepath.Join(b.dir, "product_bundles.json")
	pbs, err := util.ParseProductBundlesManifest(productBundlesPath)
	if err != nil {
		return nil, err
	}
	b.productBundles = pbs
	return b.productBundles, nil
}

func (b *FuchsiaDirBuild) GetProductBundle(
	ctx context.Context,
	productBundleName string,
) (ProductBundle, error) {
	pbs, err := b.getProductBundles()
	if err == nil {
		lookupName := productBundleName
		if lookupName == "" && pbs.Count() > 1 {
			buildInfoPath := filepath.Join(b.dir, "build_info.json")
			if defaultName, err := util.ParseBuildInfoProductBundleName(buildInfoPath); err == nil && pbs.Has(defaultName) {
				lookupName = defaultName
			}
		}

		pbPath, err := pbs.FindPath(lookupName)
		if err != nil {
			return nil, err
		}
		return &FuchsiaDirProductBundle{
			build:         b,
			productBundle: NewDirProductBundle(filepath.Join(b.dir, pbPath)),
		}, nil
	}
	if productBundleName != "" || !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	return &FuchsiaDirProductBundle{
		build: b,
	}, nil
}

type FuchsiaDirProductBundle struct {
	build         *FuchsiaDirBuild
	productBundle *DirProductBundle
}

func (pb *FuchsiaDirProductBundle) String() string {
	if pb.productBundle != nil {
		return pb.productBundle.Dir()
	}
	return pb.build.dir
}

func (pb *FuchsiaDirProductBundle) Build() Build {
	return pb.build
}

func (pb *FuchsiaDirProductBundle) GetProductBundleDir(ctx context.Context) (string, error) {
	if pb.productBundle == nil {
		return "", fmt.Errorf("no product bundle in %s: %w", pb.build.dir, os.ErrNotExist)
	}
	return pb.productBundle.Dir(), nil
}

func (pb *FuchsiaDirProductBundle) GetPackageRepository(
	ctx context.Context,
	blobFetchMode BlobFetchMode,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
	hostFfx *ffx.FFXTool,
) (*packages.Repository, error) {
	// hostFfx is ignored because FuchsiaDirBuild uses the ffx binary from
	// its own local build directory. Unlike ArtifactsBuild, it does not
	// download ffx and does not benefit from the host tool optimization.
	ffxTool, err := pb.build.GetFfx(ctx, ffxRunDir, version)
	if err != nil {
		return nil, fmt.Errorf("failed to get ffx: %w", err)
	}

	if pb.productBundle != nil {
		return pb.productBundle.GetPackageRepository(ctx, ffxTool)
	}
	logger.Warningf(ctx, "no product bundle configured, falling back to amber-files")

	blobFS := packages.NewDirBlobStore(filepath.Join(pb.build.dir, "amber-files", "repository", "blobs"))
	blobType, err := build.GetDeliveryBlobType(filepath.Join(pb.build.dir, "delivery_blob_config.json"))
	if err != nil {
		return nil, fmt.Errorf("failed to get delivery blob type: %w", err)
	}
	return packages.NewRepository(ctx, filepath.Join(pb.build.dir, "amber-files"), blobFS, ffxTool, blobType)
}

func (pb *FuchsiaDirProductBundle) GetVbmetaPath(ctx context.Context) (string, error) {
	path, err := pb.getVbmetaPathFromProductBundle(ctx)
	if err == nil {
		return path, nil
	}

	logger.Warningf(ctx, "failed to get vbmeta from product bundle, trying images: %v", err)
	return pb.build.getVbmetaPathFromImages(ctx)
}

func (pb *FuchsiaDirProductBundle) getVbmetaPathFromProductBundle(ctx context.Context) (string, error) {
	if pb.productBundle == nil {
		return "", fmt.Errorf("no product bundle in %s: %w", pb.build.dir, os.ErrNotExist)
	}

	return pb.productBundle.GetVbmetaPath()
}

func (b *FuchsiaDirBuild) GetPaverDir(ctx context.Context) (string, error) {
	return b.dir, nil
}

func (b *FuchsiaDirBuild) GetPaver(
	ctx context.Context,
	sshPublicKey ssh.PublicKey,
) (paver.Paver, error) {
	bootserverPath, err := b.GetBootserver(ctx)
	if err != nil {
		return nil, err
	}

	return paver.NewBuildPaver(bootserverPath, b.dir, paver.SSHPublicKey(sshPublicKey))
}

func (b *FuchsiaDirBuild) getVbmetaPathFromImages(ctx context.Context) (string, error) {
	imagesJSON := filepath.Join(b.dir, paver.ImageManifest)
	f, err := os.Open(imagesJSON)
	if err != nil {
		return "", fmt.Errorf("failed to open %q: %w", imagesJSON, err)
	}
	defer f.Close()

	var items []build.Image
	if err := json.NewDecoder(f).Decode(&items); err != nil {
		return "", fmt.Errorf("failed to parse vbmeta imagesJSON %q: %w", imagesJSON, err)
	}

	for _, item := range items {
		if item.Name == "zircon-a" && item.Type == "vbmeta" {
			return filepath.Join(b.dir, item.Path), nil
		}
	}

	return "", fmt.Errorf("failed to file zircon-a vbmeta in %q", imagesJSON)
}

type ProductBundleDirBuild struct {
	productBundle *DirProductBundle
	outputDir     string
}

func NewProductBundleDirBuild(
	productBundleDir string,
) *ProductBundleDirBuild {
	return &ProductBundleDirBuild{
		productBundle: NewDirProductBundle(productBundleDir),
	}
}

func (b *ProductBundleDirBuild) String() string {
	return b.productBundle.Dir()
}

func (b *ProductBundleDirBuild) Build() Build {
	return b
}

func (b *ProductBundleDirBuild) OutputDir() string {
	return b.outputDir
}

func (b *ProductBundleDirBuild) GetBootserver(ctx context.Context) (string, error) {
	// Only flashing is supported for ProductBundle
	return "", fmt.Errorf("paving is not supported with product bundles")
}

func (b *ProductBundleDirBuild) GetFfx(
	ctx context.Context,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
) (*ffx.FFXTool, error) {
	ffxPath := filepath.Join(b.productBundle.Dir(), "ffx")
	return ffx.NewFFXToolForVersion(ctx, ffxPath, ffxRunDir, version, "")
}

func (b *ProductBundleDirBuild) GetFlashManifest(ctx context.Context) (string, error) {
	return b.productBundle.Dir(), nil
}

func (b *ProductBundleDirBuild) GetProductBundle(
	ctx context.Context,
	productBundleName string,
) (ProductBundle, error) {
	return b, nil
}

func (b *ProductBundleDirBuild) GetProductBundleDir(ctx context.Context) (string, error) {
	return b.productBundle.Dir(), nil
}

func (b *ProductBundleDirBuild) GetPackageRepository(
	ctx context.Context,
	blobFetchMode BlobFetchMode,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
	hostFfx *ffx.FFXTool,
) (*packages.Repository, error) {
	// hostFfx is ignored because ProductBundleDirBuild uses the ffx binary
	// from the local product bundle directory. It does not download ffx
	// and does not benefit from the host tool optimization.
	ffxTool, err := b.GetFfx(ctx, ffxRunDir, version)
	if err != nil {
		return nil, fmt.Errorf("failed to get ffx: %w", err)
	}

	return b.productBundle.GetPackageRepository(ctx, ffxTool)
}

func (b *ProductBundleDirBuild) GetPaverDir(ctx context.Context) (string, error) {
	// Only flashing is supported for product bundles
	return "", fmt.Errorf("paving is not supported with product bundles")
}

func (b *ProductBundleDirBuild) GetPaver(
	ctx context.Context,
	sshPublicKey ssh.PublicKey,
) (paver.Paver, error) {
	// Only flashing is supported for product bundles
	return nil, fmt.Errorf("paving is not supported with product bundles")
}

func (b *ProductBundleDirBuild) GetVbmetaPath(ctx context.Context) (string, error) {
	return b.productBundle.GetVbmetaPath()
}

type OmahaBuild struct {
	productBundle ProductBundle
	omahatool     *omaha_tool.OmahaTool
	avbtool       *avb.AVBTool
	zbitool       *zbi.ZBITool
}

func NewOmahaBuild(
	productBundle ProductBundle,
	omahatool *omaha_tool.OmahaTool,
	avbtool *avb.AVBTool,
	zbitool *zbi.ZBITool,
) *OmahaBuild {
	return &OmahaBuild{
		productBundle: productBundle,
		omahatool:     omahatool,
		avbtool:       avbtool,
		zbitool:       zbitool,
	}
}

func (b *OmahaBuild) String() string {
	return b.productBundle.String()
}

func (b *OmahaBuild) Build() Build {
	return b
}

func (b *OmahaBuild) OutputDir() string {
	return b.productBundle.Build().OutputDir()
}

func (b *OmahaBuild) GetBootserver(ctx context.Context) (string, error) {
	return b.productBundle.Build().GetBootserver(ctx)
}

func (b *OmahaBuild) GetFfx(
	ctx context.Context,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
) (*ffx.FFXTool, error) {
	return b.productBundle.Build().GetFfx(ctx, ffxRunDir, version)
}

func (b *OmahaBuild) GetProductBundle(
	ctx context.Context,
	productBundleName string,
) (ProductBundle, error) {
	return b, nil
}

type versionedFlashManifest struct {
	Version  int           `json:"version"`
	Manifest flashManifest `json:"manifest"`
}

type flashManifest struct {
	Credentials []string       `json:"credentials,omitempty"`
	HwRevision  string         `json:"hw_revision"`
	Products    []flashProduct `json:"products,omitempty"`
}

type flashProduct struct {
	BootloaderPartitions []flashPartition `json:"bootloader_partitions,omitempty"`
	Name                 string           `json:"name"`
	Partitions           []flashPartition `json:"partitions,omitempty"`
	RequiresUnlock       bool             `json:"requires_unlock"`
}

type flashPartition struct {
	Name      string          `json:"name"`
	Path      string          `json:"path"`
	Condition *flashCondition `json:"condition,omitempty"`
}

type flashCondition struct {
	Value    string `json:"value"`
	Variable string `json:"variable"`
}

func (b *OmahaBuild) GetFlashManifest(ctx context.Context) (string, error) {
	flashManifestPath, err := b.productBundle.Build().GetFlashManifest(ctx)
	if err != nil {
		return "", err
	}

	fileInfo, err := os.Stat(flashManifestPath)
	if err != nil {
		return "", err
	}

	// Error out if we're dealing with a product bundle build.
	if fileInfo.IsDir() {
		return "", fmt.Errorf(
			"flashing product bundles with omaha builds currently not supported",
		)
	}

	f, err := os.Open(flashManifestPath)
	if err != nil {
		return "", err
	}
	defer f.Close()

	var manifest versionedFlashManifest
	decoder := json.NewDecoder(f)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&manifest); err != nil {
		return "", err
	}

	if manifest.Version != 3 {
		return "", fmt.Errorf("Unknown flash manifest version %d", manifest.Version)
	}

	srcVbmetaPath, err := b.GetVbmetaPath(ctx)
	if err != nil {
		return "", fmt.Errorf("failed to find zircon-a vbmeta: %w", err)
	}

	paverDir, err := b.GetPaverDir(ctx)
	if err != nil {
		return "", err
	}

	destVbmetaPath := filepath.Join(paverDir, "zircon-a-omaha-test.vbmeta")

	if err := b.updateVBMeta(ctx, srcVbmetaPath, destVbmetaPath); err != nil {
		return "", err
	}

	// Update the manifest to point at the new vbmeta.
	for i, product := range manifest.Manifest.Products {
		if product.Name == "fuchsia" || product.Name == "fuchsia_only" {
			for j, partition := range product.Partitions {
				if partition.Name == "vbmeta_a" || partition.Name == "vbmeta_b" {
					manifest.Manifest.Products[i].Partitions[j].Path = destVbmetaPath
				}
			}
		}
	}

	// Write out the manifest to a new file and return it..
	updatedFlashManifest := filepath.Join(paverDir, "flash-omaha-test.json")
	f, err = os.Create(updatedFlashManifest)
	if err != nil {
		return "", err
	}
	defer f.Close()

	encoder := json.NewEncoder(f)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(&manifest); err != nil {
		return "", err
	}

	return updatedFlashManifest, nil
}

func (b *OmahaBuild) GetProductBundleDir(ctx context.Context) (string, error) {
	productBundleDir, err := b.productBundle.GetProductBundleDir(ctx)
	if err != nil {
		return "", fmt.Errorf("failed to get product bundle dir for %s: %w", b, err)
	}

	// We need to copy the product bundle so we can overwrite the vbmeta to
	// inject the omaha url into the vbmeta.
	newProductBundleDir := filepath.Join(b.OutputDir(), "product_bundle_ota_test")

	if err := util.CopyDir(ctx, newProductBundleDir, productBundleDir); err != nil {
		return "", err
	}

	productBundle, err := util.ParseProductBundle(newProductBundleDir)
	if err != nil {
		return "", err
	}

	srcVbmetaPath, err := productBundle.GetSystemAImage("vbmeta", "zircon-a")
	if err != nil {
		return "", err
	}

	relativeDestVbmetaPath := "zircon-a-omaha-test.vbmeta"
	destVbmetaPath := filepath.Join(newProductBundleDir, relativeDestVbmetaPath)

	if err := b.updateVBMeta(ctx, srcVbmetaPath, destVbmetaPath); err != nil {
		return "", err
	}

	for i, image := range productBundle.SystemA {
		if image.Type == "vbmeta" && image.Name == "zircon-a" {
			productBundle.SystemA[i].Path = relativeDestVbmetaPath
		}
	}

	if err := util.UpdateProductBundle(newProductBundleDir, productBundle); err != nil {
		return "", err
	}

	return newProductBundleDir, nil
}

func (b *OmahaBuild) updateVBMeta(
	ctx context.Context,
	srcVbmetaPath string,
	destVbmetaPath string,
) error {
	// Create a ZBI with the omaha_url argument.
	tempDir, err := os.MkdirTemp("", "")
	if err != nil {
		return fmt.Errorf("failed to create temp directory: %w", err)
	}
	defer os.RemoveAll(tempDir)

	// Create a ZBI with the omaha_url argument.
	destZbiPath := path.Join(tempDir, "omaha_argument.zbi")
	imageArguments := map[string]string{
		"omaha_url":    b.omahatool.URL(),
		"omaha_app_id": b.omahatool.Args.AppId,
		"ota_channel":  "ota-test-channel",
	}

	if err := b.zbitool.MakeImageArgsZbi(ctx, destZbiPath, imageArguments); err != nil {
		return fmt.Errorf("Failed to create ZBI: %w", err)
	}

	// Create a vbmeta that includes the ZBI we just created.
	propFiles := map[string]string{
		"zbi": destZbiPath,
	}

	err = b.avbtool.MakeVBMetaImage(ctx, destVbmetaPath, srcVbmetaPath, propFiles)
	if err != nil {
		return fmt.Errorf("failed to create vbmeta: %w", err)
	}

	return nil
}

// GetPackageRepository returns a Repository for this product bundle.
func (b *OmahaBuild) GetPackageRepository(
	ctx context.Context,
	blobFetchMode BlobFetchMode,
	ffxRunDir ffx.RunDir,
	version ffx.FfxVersionPolicy,
	hostFfx *ffx.FFXTool,
) (*packages.Repository, error) {
	return b.productBundle.GetPackageRepository(ctx, blobFetchMode, ffxRunDir, version, hostFfx)
}

func (b *OmahaBuild) GetPaverDir(ctx context.Context) (string, error) {
	return b.productBundle.Build().GetPaverDir(ctx)
}

// GetPaver downloads and returns a paver for the build.
func (b *OmahaBuild) GetPaver(
	ctx context.Context,
	sshPublicKey ssh.PublicKey,
) (paver.Paver, error) {
	paverDir, err := b.GetPaverDir(ctx)
	if err != nil {
		return nil, err
	}
	bootserverPath, err := b.GetBootserver(ctx)
	if err != nil {
		return nil, err
	}

	srcVbmetaPath, err := b.GetVbmetaPath(ctx)
	if err != nil {
		return nil, fmt.Errorf("failed to find zircon-a vbmeta: %w", err)
	}

	destVbmetaPath := filepath.Join(paverDir, "zircon-a-omaha-test.vbmeta")
	if err := b.updateVBMeta(ctx, srcVbmetaPath, destVbmetaPath); err != nil {
		return nil, err
	}

	return paver.NewBuildPaver(
		bootserverPath,
		paverDir,
		paver.SSHPublicKey(sshPublicKey),
		paver.OverrideVBMetaA(destVbmetaPath),
	)
}

func (b *OmahaBuild) GetVbmetaPath(ctx context.Context) (string, error) {
	return b.productBundle.GetVbmetaPath(ctx)
}
