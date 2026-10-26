// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package artifactory

import (
	"archive/tar"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
)

// Implements productBundlesModules.
type mockProductBundlesModules struct {
	productBundles []build.ProductBundle
	buildDir       string
}

func (m mockProductBundlesModules) BuildDir() string {
	return m.buildDir
}

func (m mockProductBundlesModules) ProductBundles() []build.ProductBundle {
	return m.productBundles
}

func TestProductBundle2Uploads(t *testing.T) {
	transferManifest := TransferManifest{
		Version: "1",
		Entries: []TransferManifestEntry{
			{
				Type:   "files",
				Local:  "",
				Remote: "",
				Entries: []ArtifactEntry{
					{
						Name: "123",
					},
					{
						Name: "sub/456",
					},
					{
						Name: "sub/sub/789",
					},
				},
			},
			{
				Type:   "blobs",
				Local:  "blobs/1",
				Remote: "1",
				Entries: []ArtifactEntry{
					{
						Name: "abc",
					},
					{
						Name: "def",
					},
					{
						Name: "ghi",
					},
				},
			},
		},
	}
	dir := t.TempDir()
	transferManifestName := "transfer.json"
	if err := jsonutil.WriteToFile(filepath.Join(dir, transferManifestName), transferManifest); err != nil {
		t.Fatalf("failed to write to fake transfer.json file: %v", err)
	}

	m := &mockProductBundlesModules{
		productBundles: []build.ProductBundle{
			{
				Name:                 "my-product-bundle",
				Path:                 dir,
				TransferManifestPath: transferManifestName,
			},
		},
		buildDir: dir,
	}

	expectedTransferManifest := []byte(`{
  "version": "1",
  "entries": [
    {
      "type": "files",
      "local": "",
      "remote": "PRODUCT_BUNDLES/my-product-bundle",
      "entries": [
        {
          "name": "123"
        },
        {
          "name": "sub/456"
        },
        {
          "name": "sub/sub/789"
        }
      ]
    },
    {
      "type": "blobs",
      "local": "blobs/1",
      "remote": "BLOBS/1",
      "entries": [
        {
          "name": "abc"
        },
        {
          "name": "def"
        },
        {
          "name": "ghi"
        }
      ]
    }
  ]
}`)
	expected := []Upload{
		{
			Source:      filepath.Join(dir, "123"),
			Destination: filepath.Join("PRODUCT_BUNDLES", "my-product-bundle", "123"),
			Signed:      true,
		},
		{
			Source:      filepath.Join(dir, "sub", "456"),
			Destination: filepath.Join("PRODUCT_BUNDLES", "my-product-bundle", "sub", "456"),
			Signed:      true,
		},
		{
			Source:      filepath.Join(dir, "sub", "sub", "789"),
			Destination: filepath.Join("PRODUCT_BUNDLES", "my-product-bundle", "sub", "sub", "789"),
			Signed:      true,
		},
		{
			Source:      filepath.Join(dir, "blobs/1"),
			Destination: filepath.Join("BLOBS/1"),
			Deduplicate: true,
			Signed:      true,
		},
		{
			Compress:    true,
			Contents:    []byte(expectedTransferManifest),
			Destination: filepath.Join("PRODUCT_BUNDLES", "my-product-bundle", "transfer.json"),
			Signed:      true,
		},
	}

	t.Run("unset UEFIDiskPath", func(t *testing.T) {
		// Even if a disk.raw exists alongside transfer.json, it should not be
		// uploaded unless UEFIDiskPath is configured in product_bundles.json.
		if err := os.WriteFile(filepath.Join(dir, gceImageName), []byte("ignored"), 0o600); err != nil {
			t.Fatalf("failed to write stray disk.raw: %v", err)
		}
		m.productBundles[0].UEFIDiskPath = ""

		actual, err := productBundle2Uploads(m, "BLOBS", "PRODUCT_BUNDLES")
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if !reflect.DeepEqual(actual, expected) {
			t.Fatalf("unexpected product bundle uploads:\nexpected: %v\nactual: %v\n", expected, actual)
		}
	})

	uefiDiskRelPath := filepath.Join("uefi_disks", "my-product-bundle", gceImageName)

	t.Run("UEFIDiskPath not built", func(t *testing.T) {
		m.productBundles[0].UEFIDiskPath = uefiDiskRelPath

		actual, err := productBundle2Uploads(m, "BLOBS", "PRODUCT_BUNDLES")
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if !reflect.DeepEqual(actual, expected) {
			t.Fatalf("unexpected product bundle uploads when disk not built:\nexpected: %v\nactual: %v\n", expected, actual)
		}
	})

	t.Run("UEFIDiskPath built", func(t *testing.T) {
		diskContent := []byte("fake uefi disk")
		diskPath := filepath.Join(dir, uefiDiskRelPath)
		if err := os.MkdirAll(filepath.Dir(diskPath), 0o700); err != nil {
			t.Fatalf("failed to create disk dir: %v", err)
		}
		if err := os.WriteFile(diskPath, diskContent, 0o600); err != nil {
			t.Fatalf("failed to write fake disk.raw: %v", err)
		}
		m.productBundles[0].UEFIDiskPath = uefiDiskRelPath

		expectedWithDisk := append(append([]Upload(nil), expected...), Upload{
			Source:      diskPath,
			Destination: filepath.Join("PRODUCT_BUNDLES", "my-product-bundle", gceUploadName),
			Compress:    true,
			Signed:      true,
			TarHeader: &tar.Header{
				Format: tar.FormatGNU,
				Name:   gceImageName,
				Mode:   0666,
				Size:   int64(len(diskContent)),
			},
		})

		actualWithDisk, err := productBundle2Uploads(m, "BLOBS", "PRODUCT_BUNDLES")
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if !reflect.DeepEqual(actualWithDisk, expectedWithDisk) {
			t.Fatalf("unexpected product bundle uploads with GCE disk:\nexpected: %v\nactual: %v\n", expectedWithDisk, actualWithDisk)
		}
	})
}
