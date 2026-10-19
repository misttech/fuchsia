// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package util

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/build"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/productbundle"
)

// RunCommand executes a command on the host and returns the stdout and stderr
// as byte strings.
func RunCommand(ctx context.Context, name string, arg ...string) ([]byte, []byte, error) {
	logger.Infof(ctx, "running: %s %q", name, arg)
	c := exec.CommandContext(ctx, name, arg...)
	var o bytes.Buffer
	var e bytes.Buffer
	c.Stdout = &o
	c.Stderr = &e
	err := c.Run()
	stdout := o.Bytes()
	stderr := e.Bytes()
	return stdout, stderr, err
}

// Untar untars a tar.gz file into a directory.
func Untar(ctx context.Context, dst string, src string) error {
	logger.Infof(ctx, "untarring %s into %s", src, dst)

	f, err := os.Open(src)
	if err != nil {
		return err
	}
	defer f.Close()

	gz, err := gzip.NewReader(f)
	if err != nil {
		return err
	}
	defer gz.Close()

	tr := tar.NewReader(gz)

	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
			header, err := tr.Next()
			if err == io.EOF {
				return nil
			} else if err != nil {
				return err
			}

			if err := untarNext(dst, tr, header); err != nil {
				return err
			}
		}
	}
}

func untarNext(dst string, tr *tar.Reader, header *tar.Header) error {
	path := filepath.Join(dst, header.Name)
	info := header.FileInfo()
	if info.IsDir() {
		if err := os.MkdirAll(path, info.Mode()); err != nil {
			return err
		}
	} else {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return err
		}

		// Skip entry if path already exists.
		if _, err := os.Stat(path); err == nil {
			return nil
		}

		err := AtomicallyWriteFile(path, info.Mode(), func(f *os.File) error {
			_, err := io.Copy(f, tr)
			return err
		})
		if err != nil {
			return err
		}
	}

	return nil
}

// A StringOr1 is a string that can be unmarshalled from a JSON field that is
// either a string or the number 1. If the field is the number 1 it will be
// converted to the string "1".
type StringOr1 string

func (fi *StringOr1) UnmarshalJSON(b []byte) error {
	if len(b) > 0 && b[0] != '"' {
		var i int
		if err := json.Unmarshal(b, &i); err != nil {
			return err
		}
		if i == 1 {
			*fi = StringOr1("1")
			return nil
		}
		return fmt.Errorf("if version is an int it must be 1, but was %v", i)
	}
	var s string
	if err := json.Unmarshal(b, &s); err != nil {
		return err
	}
	*fi = StringOr1(s)
	return nil
}

type PackageJSON struct {
	// The update package initially used an int instead of a string for the
	// version, and we test that we are still able to update from a build
	// from that era, so we need to be able to read both formats.
	Version StringOr1 `json:"version"`
	Content []string  `json:"content"`
}

func DecodePackagesJSON(rd io.Reader) (*PackageJSON, error) {
	var p PackageJSON

	if err := json.NewDecoder(rd).Decode(&p); err != nil {
		return nil, err
	}

	if p.Version == "" {
		return nil, errors.New("version is required in packages.json format")
	}

	if p.Version != "1" {
		return nil, fmt.Errorf("packages.json version 1 is supported; found version %s", p.Version)
	}

	return &p, nil
}

// ParsePackagesJSON parses an update package's packages.json file for the
// express purpose of returning a map of package names and variant keys
// to the package's Merkle root as a value. This mimics the behavior of the
// function that parsed the legacy "packages" file format.
func ParsePackagesJSON(rd io.Reader) (map[string]build.MerkleRoot, error) {
	p, err := DecodePackagesJSON(rd)
	if err != nil {
		return nil, err
	}

	packages := make(map[string]build.MerkleRoot)
	for _, pkgURL := range p.Content {
		u, err := url.Parse(pkgURL)
		if err != nil {
			return nil, err
		}

		if u.Scheme != "fuchsia-pkg" {
			return nil, fmt.Errorf("%s is not a fuchsia-pkg URL", pkgURL)
		}

		// Path is optional and if it exists, the variant is also optional.
		if u.Path != "" {
			pathComponents := strings.Split(u.Path, "/")
			if len(pathComponents) >= 1 {
				if hash, ok := u.Query()["hash"]; ok {
					merkle, err := build.DecodeMerkleRoot([]byte(hash[0]))
					if err != nil {
						return nil, err
					}

					packages[u.Path[1:]] = merkle
				} else {
					return nil, fmt.Errorf("package %s doesn't have a merkle", u.Path[1:])
				}
			}
		}
	}

	return packages, nil
}

// Replace the host of all package URLs in an update package's packages.json file.
func RehostPackagesJSON(rd io.Reader, w io.Writer, newHostname string) error {
	p, err := DecodePackagesJSON(rd)
	if err != nil {
		return err
	}

	for i, pkgURL := range p.Content {
		u, err := url.Parse(pkgURL)
		if err != nil {
			return err
		}

		u.Host = newHostname

		p.Content[i] = u.String()
	}

	return json.NewEncoder(w).Encode(p)
}

func UpdateHashValuePackagesJSON(
	path string,
	repoName string,
	pkgUrlPath string,
	merkle build.MerkleRoot,
) error {
	if err := AtomicallyWriteFile(path, 0o600, func(f *os.File) error {
		src, err := os.Open(path)
		if err != nil {
			return fmt.Errorf("failed to open packages.json %q: %w", path, err)
		}
		defer src.Close()

		p, err := DecodePackagesJSON(src)
		if err != nil {
			return err
		}

		for i, pkgURL := range p.Content {
			u, err := url.Parse(pkgURL)
			if err != nil {
				return err
			}

			if u.Host == repoName && u.Path == "/"+pkgUrlPath {
				queryValues := u.Query()
				queryValues.Set("hash", merkle.String())
				u.RawQuery = queryValues.Encode()
			}

			p.Content[i] = u.String()
		}

		return json.NewEncoder(f).Encode(p)
	}); err != nil {
		return fmt.Errorf("failed to atomically overwrite %q: %w", path, err)
	}

	return nil
}

func ParseImagesJSON(rd io.Reader) (ImagesManifest, error) {
	var i ImagesManifest

	if err := json.NewDecoder(rd).Decode(&i); err != nil {
		return ImagesManifest{}, err
	}

	if i.Version != "1" {
		return ImagesManifest{}, fmt.Errorf("images.json version 1 is supported; found version %s", i.Version)
	}

	return i, nil
}

func UpdateImagesJSON(
	path string,
	images ImagesManifest,
) error {
	return AtomicallyWriteFile(path, 0o600, func(f *os.File) error {
		return json.NewEncoder(f).Encode(images)
	})
}

func Sha256File(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", nil
	}
	defer f.Close()

	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}

	return hex.EncodeToString(h.Sum(nil)), nil
}

// ParsePackageUrl parses a string into a URL and a MerkleRoot
func ParsePackageUrl(urlString string) (*url.URL, build.MerkleRoot, error) {
	url, err := url.Parse(urlString)
	if err != nil {
		return nil, build.MerkleRoot{}, fmt.Errorf("could not parse url %s: %w", urlString, err)
	}

	merkleString := url.Query()["hash"]
	merkle, err := build.DecodeMerkleRoot([]byte(merkleString[0]))
	if err != nil {
		return nil, build.MerkleRoot{}, fmt.Errorf("failed to decode merkle from %s: %w", urlString, err)
	}

	return url, merkle, nil
}

type ImagesManifest struct {
	Version  string        `json:"version"`
	Contents ImagesContent `json:"contents"`
}

func (i *ImagesManifest) Clone() ImagesManifest {
	partitions := []ImagePartition{}
	for _, p := range i.Contents.Partitions {
		partitions = append(partitions, p)
	}

	firmware := []ImageFirmware{}
	for _, f := range i.Contents.Firmware {
		firmware = append(firmware, f)
	}

	return ImagesManifest{
		Version: i.Version,
		Contents: ImagesContent{
			Partitions: partitions,
			Firmware:   firmware,
		},
	}
}

func (i *ImagesManifest) Rehost(newHostName string) error {
	for idx, p := range i.Contents.Partitions {
		u, err := url.Parse(p.Url)
		if err != nil {
			return err
		}

		u.Host = newHostName
		p.Url = u.String()

		i.Contents.Partitions[idx] = p
	}

	for idx, f := range i.Contents.Firmware {
		u, err := url.Parse(f.Url)
		if err != nil {
			return err
		}

		u.Host = newHostName
		f.Url = u.String()

		i.Contents.Firmware[idx] = f
	}

	return nil
}

func (i *ImagesManifest) GetPartition(slot string, typ string) (*url.URL, build.MerkleRoot, error) {
	found := false
	var partition ImagePartition
	for _, p := range i.Contents.Partitions {
		if p.Slot == slot && p.Type == typ {
			found = true
			partition = p
			break
		}
	}
	if !found {
		return nil, build.MerkleRoot{}, fmt.Errorf("missing partition in slot %s type %s", slot, typ)
	}

	return ParsePackageUrl(partition.Url)
}

func (i *ImagesManifest) SetPartition(slot string, typ string, url string, path string) error {
	fi, err := os.Stat(path)
	if err != nil {
		return err
	}
	size := fi.Size()

	hash, err := Sha256File(path)
	if err != nil {
		return err
	}

	for idx := 0; idx < len(i.Contents.Partitions); idx++ {
		p := &i.Contents.Partitions[idx]

		if p.Slot == slot && p.Type == typ {
			p.Url = url
			p.Size = size
			p.Hash = hash
			return nil
		}
	}

	return fmt.Errorf("could not find partition %s %s", slot, typ)
}

type ImagesContent struct {
	Partitions []ImagePartition `json:"partitions"`
	Firmware   []ImageFirmware  `json:"firmware"`
}

type ImagePartition struct {
	Slot string `json:"slot"`
	Type string `json:"type"`
	Size int64  `json:"size"`
	Hash string `json:"hash"`
	Url  string `json:"url"`
}

type ImageFirmware struct {
	Type string `json:"type"`
	Size int64  `json:"size"`
	Hash string `json:"hash"`
	Url  string `json:"url"`
}

func ParseProductBundle(path string) (*productbundle.ProductBundle, error) {
	productBundle, err := productbundle.Load(path)
	if err != nil {
		return nil, err
	}

	if productBundle.Version != "2" {
		return nil, fmt.Errorf("Unknown product bundle version %s", productBundle.Version)
	}

	return productBundle, nil
}

// ParseProductBundleRepository parses product_bundle.json in pbDir and returns
// the local blobs directory path and optional delivery blob type for its first
// repository.
func ParseProductBundleRepository(pbDir string) (string, *int, error) {
	productBundle, err := ParseProductBundle(pbDir)
	if err != nil {
		return "", nil, fmt.Errorf("failed to parse product bundle in %s: %w", pbDir, err)
	}
	if len(productBundle.Repositories) == 0 {
		return "", nil, fmt.Errorf("product bundle in %s has no repositories", pbDir)
	}

	repo := productBundle.Repositories[0]
	var blobType *int
	if repo.DeliveryBlobType != 0 {
		t := int(repo.DeliveryBlobType)
		blobType = &t
	}

	return filepath.Join(pbDir, repo.BlobsPath), blobType, nil
}

// ParseTransferManifestBlobs parses a product bundle transfer.json manifest and
// returns the remote paths (e.g. "blobs/1/<merkle>") for all blob entries.
func ParseTransferManifestBlobs(transferManifestPath string) ([]string, error) {
	f, err := os.Open(transferManifestPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open transfer manifest %s: %w", transferManifestPath, err)
	}
	defer f.Close()

	var transferManifest struct {
		Version string `json:"version"`
		Entries []struct {
			Type    string `json:"type"`
			Remote  string `json:"remote"`
			Entries []struct {
				Name string `json:"name"`
			} `json:"entries"`
		} `json:"entries"`
	}
	if err := json.NewDecoder(f).Decode(&transferManifest); err != nil {
		return nil, fmt.Errorf("failed to decode transfer manifest %s: %w", transferManifestPath, err)
	}
	if transferManifest.Version != "1" {
		return nil, fmt.Errorf("transfer.json version 1 is supported; found version %s", transferManifest.Version)
	}

	var blobs []string
	for _, entry := range transferManifest.Entries {
		if entry.Type == "blobs" {
			for _, blob := range entry.Entries {
				blobs = append(blobs, path.Join(entry.Remote, blob.Name))
			}
		}
	}
	return blobs, nil
}

// ProductBundles maps product bundle names to their relative paths within a build.
type ProductBundles struct {
	paths map[string]string
}

func NewProductBundles(paths map[string]string) *ProductBundles {
	return &ProductBundles{paths: paths}
}

// ParseProductBundlesManifest parses a product_bundles.json manifest file.
func ParseProductBundlesManifest(productBundlesPath string) (*ProductBundles, error) {
	f, err := os.Open(productBundlesPath)
	if err != nil {
		return nil, fmt.Errorf("failed to open %s: %w", productBundlesPath, err)
	}
	defer f.Close()

	var productBundles []struct {
		Name string `json:"name"`
		Path string `json:"path"`
	}
	if err := json.NewDecoder(f).Decode(&productBundles); err != nil {
		return nil, fmt.Errorf("failed to parse product bundles path %s: %w", productBundlesPath, err)
	}

	paths := make(map[string]string, len(productBundles))
	for _, pb := range productBundles {
		paths[pb.Name] = pb.Path
	}

	return NewProductBundles(paths), nil
}

// NewProductBundlesFromArchiveSrcs extracts the product bundles present in a
// build archive's file listing (under "product_bundles/<name>/<manifestName>").
func NewProductBundlesFromArchiveSrcs(srcs map[string]struct{}, manifestName string) *ProductBundles {
	paths := make(map[string]string)
	suffix := "/" + manifestName
	for src := range srcs {
		if strings.HasPrefix(src, "product_bundles/") && strings.HasSuffix(src, suffix) {
			name := strings.TrimSuffix(strings.TrimPrefix(src, "product_bundles/"), suffix)
			if name != "" && !strings.Contains(name, "/") {
				paths[name] = path.Join("product_bundles", name)
			}
		}
	}
	return NewProductBundles(paths)
}

func (pbs *ProductBundles) Count() int {
	return len(pbs.paths)
}

func (pbs *ProductBundles) Has(productBundleName string) bool {
	_, ok := pbs.paths[productBundleName]
	return ok
}

func (pbs *ProductBundles) Names() []string {
	names := make([]string, 0, len(pbs.paths))
	for name := range pbs.paths {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// FindPath returns the relative path for the named product bundle (or the
// single entry when productBundleName is empty). Returns an error if no product
// bundles exist, if the named bundle is not found, or if productBundleName is
// empty and multiple product bundles are present.
func (pbs *ProductBundles) FindPath(productBundleName string) (string, error) {
	if len(pbs.paths) == 0 {
		return "", fmt.Errorf("no product bundles found")
	}

	if productBundleName != "" {
		if p, ok := pbs.paths[productBundleName]; ok {
			return p, nil
		}
		return "", fmt.Errorf(
			"product bundle %q not found (available: %s)",
			productBundleName,
			strings.Join(pbs.Names(), ", "),
		)
	}

	if len(pbs.paths) > 1 {
		return "", fmt.Errorf(
			"multiple product bundles found (%s); specify --product-bundle-name",
			strings.Join(pbs.Names(), ", "),
		)
	}

	for _, p := range pbs.paths {
		return p, nil
	}
	return "", fmt.Errorf("no product bundles found")
}

// ParseBuildInfoProductBundleName parses build_info.json and returns the
// "<product>.<board>" name for its primary configuration.
func ParseBuildInfoProductBundleName(buildInfoPath string) (string, error) {
	f, err := os.Open(buildInfoPath)
	if err != nil {
		return "", err
	}
	defer f.Close()

	var buildInfo struct {
		Configurations []struct {
			Board   string `json:"board"`
			Product string `json:"product"`
		} `json:"configurations"`
		Version string `json:"version"`
	}
	if err := json.NewDecoder(f).Decode(&buildInfo); err != nil {
		return "", fmt.Errorf("failed to read build info: %w", err)
	}
	if len(buildInfo.Configurations) == 0 {
		return "", fmt.Errorf("no configurations found in %s", buildInfoPath)
	}

	config := buildInfo.Configurations[0]
	if config.Product == "" || config.Board == "" {
		return "", fmt.Errorf("empty product or board in %s", buildInfoPath)
	}
	return fmt.Sprintf("%s.%s", config.Product, config.Board), nil
}

func UpdateProductBundle(productBundleDir string, productBundle *productbundle.ProductBundle) error {
	return AtomicallyWriteFile(
		filepath.Join(productBundleDir, "product_bundle.json"),
		0o600,
		func(f *os.File) error {
			return json.NewEncoder(f).Encode(productBundle)
		},
	)
}

func AtomicallyWriteFile(path string, mode os.FileMode, writeFileFunc func(*os.File) error) error {
	dir := filepath.Dir(path)
	basename := filepath.Base(path)

	tmpfile, err := os.CreateTemp(dir, basename)
	if err != nil {
		return err
	}
	defer func() {
		if tmpfile != nil {
			tmpfile.Close()
			os.Remove(tmpfile.Name())
		}
	}()

	if err = writeFileFunc(tmpfile); err != nil {
		return err
	}

	if err = os.Chmod(tmpfile.Name(), mode); err != nil {
		return err
	}

	// Now that we've written the file, do an atomic swap of the filename into place.
	if err := os.Rename(tmpfile.Name(), path); err != nil {
		return err
	}
	if err := tmpfile.Close(); err != nil {
		return err
	}
	tmpfile = nil

	return nil
}

// RunWithTimeout runs a closure to completion, or returns an error if it times
// out.
func RunWithTimeout(ctx context.Context, timeout time.Duration, f func() error) error {
	return RunWithDeadline(ctx, time.Now().Add(timeout), f)
}

// RunWithDeadline runs a closure to runs the closure in a goroutine
func RunWithDeadline(ctx context.Context, deadline time.Time, f func() error) error {
	ctx, cancel := context.WithDeadline(ctx, deadline)
	defer cancel()

	ch := make(chan error, 1)
	go func() {
		ch <- f()
	}()

	select {
	case err := <-ch:
		return err
	case <-ctx.Done():
		return fmt.Errorf("Function timed out: %w", ctx.Err())
	}
}

func ValidatePath(path string) error {
	if path != "" {
		if _, err := os.Stat(path); err != nil {
			if os.IsNotExist(err) {
				return fmt.Errorf("no such file or directory exists: %q", path)
			} else {
				return fmt.Errorf("unexpected error reading %q: %v", path, err)
			}
		}
	}

	return nil
}

func AddSuffixToPackageName(packagePath string, suffix string) string {
	before, after, ok := strings.Cut(packagePath, "/")
	if ok {
		return fmt.Sprintf("%s_%s/%s", before, suffix, after)
	} else {
		return fmt.Sprintf("%s_%s", before, suffix)
	}
}

func CopyDir(ctx context.Context, dst string, src string) error {
	logger.Infof(ctx, "copying %s into %s", src, dst)

	return filepath.WalkDir(src, func(path string, info os.DirEntry, err error) error {
		if err != nil {
			return err
		}

		if path == src {
			return nil
		}

		relPath, err := filepath.Rel(src, path)
		if err != nil {
			return err
		}
		dstPath := filepath.Join(dst, relPath)

		if info.IsDir() {
			return os.MkdirAll(dstPath, os.ModePerm)
		}

		if fi, err := os.Stat(dstPath); err == nil {
			if fi.IsDir() {
				// If dstPath already exists and is a directory, then return nil so
				// we can walk the contents of the directory and move over the
				// individual files.
				return nil
			}
		}
		if err = os.MkdirAll(filepath.Dir(dstPath), os.ModePerm); err != nil {
			return err
		}

		return linkOrCopy(ctx, dstPath, path)
	})
}

func linkOrCopy(ctx context.Context, dstPath string, srcPath string) error {
	if err := os.Link(srcPath, dstPath); err != nil {
		src, err := os.Open(srcPath)
		if err != nil {
			return err
		}
		defer src.Close()

		stat, err := src.Stat()
		if err != nil {
			return err
		}

		return AtomicallyWriteFile(dstPath, stat.Mode(), func(f *os.File) error {
			_, err := io.Copy(f, src)
			return err
		})
	}

	return nil
}
