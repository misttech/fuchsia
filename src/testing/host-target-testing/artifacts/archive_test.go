// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package artifacts

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"go.fuchsia.dev/fuchsia/src/testing/host-target-testing/ffx"
)

func TestDownload(t *testing.T) {
	tmpDir := t.TempDir()

	// The expected command will be something like `artifacts cp -build <id>
	// -dst <dst> -srcs-file <srcs-file>`. Make a mock script to copy src to the
	// dst directory. If src doesn't exist, create it so that a retry of the
	// command will succeed.
	mockArtifactsWithSrcsFileScript := filepath.Join(tmpDir, "artifacts_retry_srcsfile")
	if err := os.WriteFile(mockArtifactsWithSrcsFileScript, []byte(`#!/bin/bash
	set -x
	dst="$5"
	srcs_file=$(cat "$7")
	mkdir -p $dst
	cp -r $srcs_file "$dst" || (for src in $srcs_file; do echo contents > $src; done && exit 1)
	`), os.ModePerm); err != nil {
		t.Fatal(err)
	}
	srcDir := filepath.Join(tmpDir, "src_dir")
	if err := os.Mkdir(srcDir, os.ModePerm); err != nil {
		t.Fatal(err)
	}
	srcFile := filepath.Join(srcDir, "src_file")
	if err := os.WriteFile(srcFile, []byte("src"), os.ModePerm); err != nil {
		t.Fatal(err)
	}

	tests := []struct {
		name          string
		srcs          []string
		dst           string
		expectedFiles map[string]string
		forceRetry    bool
	}{
		{
			name: "download dir",
			srcs: []string{srcDir},
			dst:  "download_dir",
			expectedFiles: map[string]string{
				filepath.Join("download_dir", "src_dir", "src_file"): "src",
			},
		},
		{
			name: "download file",
			srcs: []string{srcFile},
			dst:  "download_file",
			expectedFiles: map[string]string{
				filepath.Join("download_file", "src_file"): "src",
			},
		},
		{
			name: "retry download",
			srcs: []string{filepath.Join(tmpDir, "new_src")},
			dst:  "retry_download_file",
			expectedFiles: map[string]string{
				filepath.Join("retry_download_file", "new_src"): "contents",
			},
			forceRetry: true,
		},
		{
			name: "retry download with srcs-file",
			srcs: []string{filepath.Join(tmpDir, "new_src"), filepath.Join(tmpDir, "new_src2")},
			dst:  "retry_download_srcs_file",
			expectedFiles: map[string]string{
				filepath.Join("retry_download_srcs_file", "new_src"):  "contents",
				filepath.Join("retry_download_srcs_file", "new_src2"): "contents",
			},
			forceRetry: true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			artifactsPath := mockArtifactsWithSrcsFileScript
			if test.forceRetry {
				// Make sure all srcs don't exist so that the script fails the first time.
				for _, src := range test.srcs {
					os.Remove(src)
				}
			}
			archive := NewArchive("", artifactsPath)
			tmpDstDir := filepath.Join(tmpDir, "test")
			if err := os.Mkdir(tmpDstDir, os.ModePerm); err != nil {
				t.Fatal(err)
			}
			defer os.RemoveAll(tmpDstDir)
			if err := archive.download(context.Background(), "id", false, filepath.Join(tmpDstDir, test.dst), test.srcs); err != nil {
				t.Fatal(err)
			}
			for expectedFile, expectedContents := range test.expectedFiles {
				data, err := os.ReadFile(filepath.Join(tmpDstDir, expectedFile))
				if err != nil {
					t.Error(err)
				}
				if strings.Trim(string(data), "\n") != expectedContents {
					t.Errorf("expected contents: %s, got: %s", expectedContents, data)
				}
			}
		})
	}
}

func TestDownloadNotExist(t *testing.T) {
	tmpDir := t.TempDir()
	// The expected command will be something like `artifacts cp -build <id> -src <src> -dst <dst> -srcs-file <srcs-file>`.
	// Make a mock script to always say the artifact doesn't exist.
	mockArtifactsScript := filepath.Join(tmpDir, "artifacts")
	if err := os.WriteFile(mockArtifactsScript, []byte("#!/bin/bash\necho \"object doesn't exist\" >&2\nexit 1"), os.ModePerm); err != nil {
		t.Fatal(err)
	}

	archive := NewArchive("", mockArtifactsScript)
	err := archive.download(context.Background(), "id", false, "dst", []string{"src"})
	if !os.IsNotExist(err) {
		t.Fatal("expected not exist error, got:", err)
	}
}

func TestFuchsiaDirBuildGetProductBundleDir(t *testing.T) {
	ctx := context.Background()

	// Single entry succeeds when productBundleName is empty.
	singleDir := t.TempDir()
	singleJSON := `[
		{"name": "core.arm64", "path": "obj/build/images/fuchsia/product_bundle"}
	]`
	if err := os.WriteFile(filepath.Join(singleDir, "product_bundles.json"), []byte(singleJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	b := NewFuchsiaDirBuild(singleDir)
	pb, err := b.GetProductBundle(ctx, "")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	got, err := pb.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want := filepath.Join(singleDir, "obj/build/images/fuchsia/product_bundle")
	if got != want {
		t.Errorf("got %q, want %q", got, want)
	}

	tmpDir := t.TempDir()
	pbJSON := `[
		{"name": "core.arm64", "path": "obj/build/images/fuchsia/product_bundle"},
		{"name": "workbench_eng.arm64", "path": "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle"}
	]`
	if err := os.WriteFile(filepath.Join(tmpDir, "product_bundles.json"), []byte(pbJSON), 0o644); err != nil {
		t.Fatal(err)
	}

	// Multiple entries return an error when productBundleName is empty.
	b = NewFuchsiaDirBuild(tmpDir)
	if _, err := b.GetProductBundle(ctx, ""); err == nil {
		t.Fatal("expected error for multiple product bundles without name, got nil")
	}

	// Named product bundle returns the matching entry.
	pb, err = b.GetProductBundle(ctx, "workbench_eng.arm64")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	got, err = pb.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	want = filepath.Join(tmpDir, "obj/build/images/fuchsia/workbench_eng.arm64/product_bundle")
	if got != want {
		t.Errorf("got %q, want %q", got, want)
	}

	// Non-existent product bundle returns an error.
	if _, err := b.GetProductBundle(ctx, "nonexistent"); err == nil {
		t.Fatal("expected error for nonexistent product bundle, got nil")
	}

	// Multiple entries resolve via build_info.json when productBundleName is empty.
	buildInfoJSON := `{"configurations":[{"board":"arm64","product":"workbench_eng"}],"version":"1.0.0"}`
	if err := os.WriteFile(filepath.Join(tmpDir, "build_info.json"), []byte(buildInfoJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	pb, err = b.GetProductBundle(ctx, "")
	if err != nil {
		t.Fatalf("unexpected error with build_info.json fallback: %v", err)
	}
	got, err = pb.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if got != want {
		t.Errorf("got %q, want %q", got, want)
	}
}

func TestArtifactsBuildGetProductBundleDirWithName(t *testing.T) {
	tmpDir := t.TempDir()
	mockArtifactsScript := filepath.Join(tmpDir, "artifacts")
	script := `#!/bin/bash
if [[ "$1" == "ls" ]]; then
  if [[ "$3" != "single" ]]; then
    echo "product_bundles/core.arm64/product_bundle.json"
  fi
  if [[ "$3" == "build_info_fallback" ]]; then
    echo "build_api/build_info.json"
  fi
  echo "product_bundles/workbench_eng.arm64/product_bundle.json"
  echo "product_bundles/workbench_eng.arm64/transfer.json"
  echo "product_bundles/workbench_eng.arm64/keys/root.json"
  echo "product_bundles/workbench_eng.arm64/repository/targets.json"
  exit 0
fi
if [[ "$1" == "cp" ]]; then
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -dst)
        dst="$2"
        shift 2
        ;;
      -srcs-file)
        srcs_file="$2"
        shift 2
        ;;
      *)
        shift
        ;;
    esac
  done
  for src in $(cat "$srcs_file"); do
    mkdir -p "$(dirname "$dst/$src")"
    if [[ "$src" == "build_api/build_info.json" ]]; then
      echo '{"configurations":[{"board":"arm64","product":"workbench_eng"}],"version":"1.0.0"}' > "$dst/$src"
    elif [[ "$src" == *"product_bundle.json" ]]; then
      echo '{"version":"2","product_name":"workbench_eng.arm64","product_version":"1","partitions":{},"repositories":[{"name":"fuchsia.com","metadata_path":"repository","blobs_path":"blobs","delivery_blob_type":1}]}' > "$dst/$src"
    elif [[ "$src" == *"transfer.json" ]]; then
      echo '{"version":"1","entries":[{"type":"blobs","remote":"blobs/1","entries":[{"name":"deadbeef"}]}]}' > "$dst/$src"
    else
      echo "{}" > "$dst/$src"
    fi
  done
  exit 0
fi
exit 1
`
	if err := os.WriteFile(mockArtifactsScript, []byte(script), os.ModePerm); err != nil {
		t.Fatal(err)
	}

	ctx := context.Background()
	archive := NewArchive("", mockArtifactsScript)
	b, err := archive.GetBuildByID(ctx, "12345", filepath.Join(tmpDir, "out"))
	if err != nil {
		t.Fatalf("GetBuildByID failed: %v", err)
	}
	pb, err := b.GetProductBundle(ctx, "workbench_eng.arm64")
	if err != nil {
		t.Fatalf("GetProductBundle failed: %v", err)
	}

	pbDir, err := pb.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("GetProductBundleDir failed: %v", err)
	}
	wantDir := filepath.Join(tmpDir, "out", "12345", "product_bundles", "workbench_eng.arm64")
	if pbDir != wantDir {
		t.Errorf("got %q, want %q", pbDir, wantDir)
	}
	if _, err := os.Stat(filepath.Join(pbDir, "product_bundle.json")); err != nil {
		t.Errorf("expected product_bundle.json to be downloaded: %v", err)
	}

	if _, err := pb.GetPackageRepository(ctx, LazilyFetchBlobs, ffx.RunDir{}, ffx.FfxVersionPolicyLatest, &ffx.FFXTool{}); err != nil {
		t.Fatalf("GetPackageRepository failed: %v", err)
	}
	if _, err := os.Stat(filepath.Join(pbDir, "repository", "targets.json")); err != nil {
		t.Errorf("expected targets.json in pbDir: %v", err)
	}

	deadbeefBlob := filepath.Join(tmpDir, "out", "blobs", "1", "deadbeef")
	if _, err := os.Stat(deadbeefBlob); err == nil {
		t.Errorf("expected deadbeef not to be prefetched with LazilyFetchBlobs, but found: %s", deadbeefBlob)
	}

	b2, err := archive.GetBuildByID(ctx, "67890", filepath.Join(tmpDir, "out"))
	if err != nil {
		t.Fatalf("GetBuildByID failed: %v", err)
	}
	pb2, err := b2.GetProductBundle(ctx, "workbench_eng.arm64")
	if err != nil {
		t.Fatalf("GetProductBundle failed: %v", err)
	}
	if _, err := pb2.GetPackageRepository(ctx, PrefetchBlobs, ffx.RunDir{}, ffx.FfxVersionPolicyLatest, &ffx.FFXTool{}); err != nil {
		t.Fatalf("GetPackageRepository with PrefetchBlobs failed: %v", err)
	}
	if _, err := os.Stat(deadbeefBlob); err != nil {
		t.Errorf("expected deadbeef blob to be prefetched: %v", err)
	}

	// Multiple product bundles without an explicit name returns an error.
	bMulti, err := archive.GetBuildByID(ctx, "11111", filepath.Join(tmpDir, "out"))
	if err != nil {
		t.Fatalf("GetBuildByID failed: %v", err)
	}
	if _, err := bMulti.GetProductBundle(ctx, ""); err == nil {
		t.Fatal("expected error when multiple product bundles exist and productBundleName is empty, got nil")
	}

	// Single product bundle without an explicit name resolves the default product bundle.
	bSingle, err := archive.GetBuildByID(ctx, "single", filepath.Join(tmpDir, "out"))
	if err != nil {
		t.Fatalf("GetBuildByID failed: %v", err)
	}
	pbSingle, err := bSingle.GetProductBundle(ctx, "")
	if err != nil {
		t.Fatalf("GetProductBundle failed: %v", err)
	}
	if _, err := pbSingle.GetPackageRepository(ctx, LazilyFetchBlobs, ffx.RunDir{}, ffx.FfxVersionPolicyLatest, &ffx.FFXTool{}); err != nil {
		t.Fatalf("GetPackageRepository with default product bundle resolution failed: %v", err)
	}
	wantDefaultTargets := filepath.Join(tmpDir, "out", "single", "product_bundles", "workbench_eng.arm64", "repository", "targets.json")
	if _, err := os.Stat(wantDefaultTargets); err != nil {
		t.Errorf("expected targets.json in default resolved pbDir: %v", err)
	}

	// Multiple product bundles with build_api/build_info.json resolves the default product bundle.
	bFallback, err := archive.GetBuildByID(ctx, "build_info_fallback", filepath.Join(tmpDir, "out"))
	if err != nil {
		t.Fatalf("GetBuildByID failed: %v", err)
	}
	pbFallback, err := bFallback.GetProductBundle(ctx, "")
	if err != nil {
		t.Fatalf("GetProductBundle with build_info.json fallback failed: %v", err)
	}
	pbFallbackDir, err := pbFallback.GetProductBundleDir(ctx)
	if err != nil {
		t.Fatalf("GetProductBundleDir failed: %v", err)
	}
	wantFallbackDir := filepath.Join(tmpDir, "out", "build_info_fallback", "product_bundles", "workbench_eng.arm64")
	if pbFallbackDir != wantFallbackDir {
		t.Errorf("got %q, want %q", pbFallbackDir, wantFallbackDir)
	}
}
