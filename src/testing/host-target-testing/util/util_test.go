// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package util

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/build"
)

const (
	merkleA = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	merkleB = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
)

func pkgURL(name, hash string) string {
	return fmt.Sprintf("fuchsia-pkg://host/%s?hash=%s", name, hash)
}

type pkgMerkle struct {
	name string
	hash build.MerkleRoot
}

func TestParsePackagesJSON(t *testing.T) {
	tests := []struct {
		id        string
		json      []byte
		pkgs      []pkgMerkle
		expectErr bool
	}{
		{
			id:        "invalid json fails",
			json:      []byte("abcd"),
			expectErr: true,
		},
		{
			id:        "empty object fails",
			json:      []byte("{}"),
			expectErr: true,
		},
		{
			id:        "invalid version fails",
			json:      []byte(`{"version":"42","content":[]}`),
			expectErr: true,
		},
		{
			id:        "numeric 1 succeeds",
			json:      []byte(`{"version":1,"content":[]}`),
			expectErr: false,
		},
		{
			id:        "numeric 2 fails",
			json:      []byte(`{"version":2,"content":[]}`),
			expectErr: true,
		},
		{
			id:        "contentless version succeeds",
			json:      []byte(`{"version":"1"}`),
			expectErr: false,
		},
		{
			id:        "variant-less package succeeds",
			json:      []byte(fmt.Sprintf(`{"version":"1","content":["%s"]}`, pkgURL("pkg", merkleA))),
			pkgs:      []pkgMerkle{{"pkg", build.MustDecodeMerkleRoot(merkleA)}},
			expectErr: false,
		},
		{
			id:        "variant package succeeds",
			json:      []byte(fmt.Sprintf(`{"version":"1","content":["%s"]}`, pkgURL("pkg/0", merkleA))),
			pkgs:      []pkgMerkle{{"pkg/0", build.MustDecodeMerkleRoot(merkleA)}},
			expectErr: false,
		},
		{
			id: "multiple packages succeed",
			json: []byte(fmt.Sprintf(
				`{"version":"1","content":["%s","%s"]}`,
				pkgURL("pkg/0", merkleA),
				pkgURL("another/0", merkleB)),
			),
			pkgs: []pkgMerkle{
				{"pkg/0", build.MustDecodeMerkleRoot(merkleA)},
				{"another/0", build.MustDecodeMerkleRoot(merkleB)},
			},
			expectErr: false,
		},
	}

	for _, test := range tests {
		r := bytes.NewBuffer(test.json)
		pkgs, err := ParsePackagesJSON(r)

		gotErr := (err != nil)
		if test.expectErr && !gotErr {
			t.Errorf("test %s: want error; got none", test.id)
		} else if !test.expectErr && gotErr {
			t.Errorf("test %s: want no error; got %v", test.id, err)
		}

		if len(pkgs) != len(test.pkgs) {
			t.Errorf("test %s: got %d packages; want %d", test.id, len(pkgs), len(test.pkgs))
		}

		if test.pkgs != nil {
			for _, pv := range test.pkgs {
				h, ok := pkgs[pv.name]
				if !ok {
					t.Errorf("test %s: key not found; want %s", test.id, pv.name)
				}
				if h != pv.hash {
					t.Errorf("test %s for package %s: got hash %s; want %s", test.id, pv.name, h, pv.hash)
				}
			}
		}
	}
}

func TestRehostPackagesJSON(t *testing.T) {
	tests := []struct {
		id       string
		json     []byte
		expected string
	}{
		{
			id:       "regular update package",
			json:     []byte(fmt.Sprintf(`{"version":"1","content":["%s","%s"]}`, pkgURL("pkg/0", "abc"), pkgURL("another/0", "def"))),
			expected: `{"version":"1","content":["fuchsia-pkg://new/pkg/0?hash=abc","fuchsia-pkg://new/another/0?hash=def"]}`,
		},
		{
			id:       "numeric version 1 converted to string",
			json:     []byte(`{"version":1,"content":[]}`),
			expected: `{"version":"1","content":[]}`,
		},
	}

	for _, test := range tests {
		r := bytes.NewBuffer(test.json)
		var b bytes.Buffer
		err := RehostPackagesJSON(r, &b, "new")
		if err != nil {
			t.Errorf("test %s: RehostPackagesJSON failed: %v", test.id, err)
		}
		actual := strings.TrimSpace(b.String())
		if actual != test.expected {
			t.Errorf("test %s: packages.json does not match, expected '%s', got '%s'", test.id, test.expected, actual)
		}
	}
}

func TestProductBundles(t *testing.T) {
	tmpDir := t.TempDir()

	// Single entry succeeds when productBundleName is empty.
	singleManifest := filepath.Join(tmpDir, "single_product_bundles.json")
	singleJSON := `[
		{"name": "core.arm64", "path": "obj/build/images/fuchsia/product_bundle"}
	]`
	if err := os.WriteFile(singleManifest, []byte(singleJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	singlePBs, err := ParseProductBundlesManifest(singleManifest)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	got, err := singlePBs.FindPath("")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "obj/build/images/fuchsia/product_bundle" {
		t.Errorf("got %q, want %q", got, "obj/build/images/fuchsia/product_bundle")
	}

	// Multiple entries return an error when productBundleName is empty.
	pbManifest := filepath.Join(tmpDir, "product_bundles.json")
	pbJSON := `[
		{"name": "core.arm64", "path": "obj/build/images/fuchsia/product_bundle"},
		{"name": "workbench_eng.arm64", "path": "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle"}
	]`
	if err := os.WriteFile(pbManifest, []byte(pbJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	pbs, err := ParseProductBundlesManifest(pbManifest)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if _, err := pbs.FindPath(""); err == nil {
		t.Fatal("expected error for multiple product bundles without name, got nil")
	}

	// Named entry returns matching path.
	got, err = pbs.FindPath("workbench_eng.arm64")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle" {
		t.Errorf("got %q, want %q", got, "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle")
	}

	// Unknown name returns error.
	if _, err := pbs.FindPath("nonexistent"); err == nil {
		t.Fatal("expected error for nonexistent product bundle, got nil")
	}

	// Archive srcs parsing.
	archivePBs := NewProductBundlesFromArchiveSrcs(map[string]struct{}{
		"product_bundles/core.arm64/product_bundle.json": {},
	}, "product_bundle.json")
	got, err = archivePBs.FindPath("")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "product_bundles/core.arm64" {
		t.Errorf("got %q, want %q", got, "product_bundles/core.arm64")
	}
}

func TestParseProductBundleRepository(t *testing.T) {
	tmpDir := t.TempDir()
	pbJSON := `{"version":"2","product_name":"workbench_eng.arm64","product_version":"1","partitions":{},"repositories":[{"name":"fuchsia.com","metadata_path":"repository","blobs_path":"blobs/1","delivery_blob_type":1}]}`
	if err := os.WriteFile(filepath.Join(tmpDir, "product_bundle.json"), []byte(pbJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	blobsDir, blobType, err := ParseProductBundleRepository(tmpDir)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	wantBlobsDir := filepath.Join(tmpDir, "blobs/1")
	if blobsDir != wantBlobsDir {
		t.Errorf("got blobsDir %q, want %q", blobsDir, wantBlobsDir)
	}
	if blobType == nil || *blobType != 1 {
		t.Errorf("got blobType %v, want 1", blobType)
	}

	// Empty repositories returns error.
	emptyRepoDir := t.TempDir()
	emptyRepoJSON := `{"version":"2","product_name":"workbench_eng.arm64","product_version":"1","partitions":{},"repositories":[]}`
	if err := os.WriteFile(filepath.Join(emptyRepoDir, "product_bundle.json"), []byte(emptyRepoJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, _, err := ParseProductBundleRepository(emptyRepoDir); err == nil {
		t.Fatal("expected error for empty repositories, got nil")
	}
}

func TestParseBuildInfoProductBundleName(t *testing.T) {
	tmpDir := t.TempDir()
	buildInfoPath := filepath.Join(tmpDir, "build_info.json")
	buildInfoJSON := `{"configurations":[{"board":"arm64","product":"workbench_eng"}],"version":"1.0.0"}`
	if err := os.WriteFile(buildInfoPath, []byte(buildInfoJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	got, err := ParseBuildInfoProductBundleName(buildInfoPath)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != "workbench_eng.arm64" {
		t.Errorf("got %q, want %q", got, "workbench_eng.arm64")
	}

	emptyConfigPath := filepath.Join(tmpDir, "empty_build_info.json")
	if err := os.WriteFile(emptyConfigPath, []byte(`{"configurations":[]}`), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseBuildInfoProductBundleName(emptyConfigPath); err == nil {
		t.Fatal("expected error for empty configurations, got nil")
	}

	emptyProductPath := filepath.Join(tmpDir, "empty_product_build_info.json")
	if err := os.WriteFile(emptyProductPath, []byte(`{"configurations":[{"board":"arm64","product":""}]}`), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseBuildInfoProductBundleName(emptyProductPath); err == nil {
		t.Fatal("expected error for empty product, got nil")
	}
}

func TestParseTransferManifestBlobs(t *testing.T) {
	tmpDir := t.TempDir()
	transferManifestPath := filepath.Join(tmpDir, "transfer.json")
	transferJSON := `{
		"version": "1",
		"entries": [
			{
				"type": "files",
				"remote": "files",
				"entries": [
					{"name": "file1"}
				]
			},
			{
				"type": "blobs",
				"remote": "blobs/1",
				"entries": [
					{"name": "deadbeef"},
					{"name": "feedface"}
				]
			}
		]
	}`
	if err := os.WriteFile(transferManifestPath, []byte(transferJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	blobs, err := ParseTransferManifestBlobs(transferManifestPath)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	wantBlobs := []string{
		"blobs/1/deadbeef",
		"blobs/1/feedface",
	}
	if len(blobs) != len(wantBlobs) {
		t.Fatalf("got %d blobs, want %d", len(blobs), len(wantBlobs))
	}
	for i := range wantBlobs {
		if blobs[i] != wantBlobs[i] {
			t.Errorf("got blob[%d] = %q, want %q", i, blobs[i], wantBlobs[i])
		}
	}

	// Invalid version returns error.
	invalidVersionPath := filepath.Join(tmpDir, "invalid_transfer.json")
	invalidVersionJSON := `{"version": "2", "entries": []}`
	if err := os.WriteFile(invalidVersionPath, []byte(invalidVersionJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := ParseTransferManifestBlobs(invalidVersionPath); err == nil {
		t.Fatal("expected error for unsupported version 2, got nil")
	}
}
