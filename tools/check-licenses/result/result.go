// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"fmt"
	"io"
	"log"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
)

const (
	indent = "  "
)

// SaveResults saves the results to the output files defined in the config file.
func SaveResults() error {
	var b strings.Builder

	s, err := saveReadmeFuchsiaFiles()
	if err != nil {
		return err
	}
	b.WriteString(s)

	if Config.RunAnalysis {
		func() {
			defer metrics.ChecksDuration.Track()()
			err = RunChecks()
		}()
		if err != nil {
			return err
		}
	} else {
		log.Printf(" -> Not running tests on results.\n")
	}

	if Config.OutputLicenseFile {
		s1, err := expandTemplates()
		if err != nil {
			return err
		}
		b.WriteString(s1)
	} else {
		log.Printf(" -> Not expanding templates.\n")
	}

	projectList := make([]*project.Project, 0)
	for _, p := range project.GetAllFilteredProjects() {
		projectList = append(projectList, p)
	}
	sort.Sort(project.Order(projectList))

	if Config.OutputLicenseFile {
		s, err := generateSPDXDoc(
			Config.SPDXDocName,
			projectList,
			project.RootProject,
		)
		if err != nil {
			// TODO(https://fxbug.dev/42067990): Return ("", err) instead once SPDX generation
			// stops flaking. For now, do not treat a failure from generateSPDXDoc
			// as fatal.
			b.WriteString(fmt.Sprintf("SPDX doc generation failed: %v\n", err))
		} else {
			b.WriteString(s)
		}
	} else {
		log.Printf(" -> Not generating SPDX doc.\n")
	}

	if err = writeFile("summary", []byte(b.String())); err != nil {
		return err
	}

	if Config.OutDir != "" {
		if err := compressTarGZ(Config.OutDir, path.Join(Config.RootOutDir, "runFiles")); err != nil {
			return err
		}
	}

	return nil
}

// This retrieves all the relevant metrics information for a given package.
// e.g. the //tools/check-licenses/directory package.
func saveReadmeFuchsiaFiles() (string, error) {
	var b strings.Builder

	if Config.OverwriteReadmeFiles {
		for _, p := range project.GetAllFilteredProjects() {
			if p.ReadmeFile == nil {
				continue
			}

			// Create project directory if it doesn't exist.
			dir := filepath.Dir(p.ReadmeFile.ReadmePath)
			if err := os.MkdirAll(dir, 0755); err != nil {
				return "", fmt.Errorf("Failed to make README.fuchsia file directory %s: %w", dir, err)
			}

			f, err := os.Create(p.ReadmeFile.ReadmePath)
			if err != nil {
				// TODO: Throw an error when you fail to overwrite
				// the README.fuchsia file.
				continue
			}

			// Write the contents of the string to the file
			fmt.Fprintln(f, p.ReadmeFile.String())
			f.Close()
		}
	}

	return b.String(), nil
}

// Write file to <Config.OutDir>/<path parameter>
func writeFile(path string, data []byte) error {
	return writeFileRoot(path, data, Config.OutDir)
}

// Write file to <root parameter>/<path parameter>
func writeFileRoot(path string, data []byte, root string) error {
	path = filepath.Join(root, path)
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("failed to make directory %s: %w", dir, err)
	}
	if err := os.WriteFile(path, data, 0666); err != nil {
		return fmt.Errorf("failed to write file %s: %w", path, err)
	}
	return nil
}

func compressGZ(path string) error {
	d, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("failed to read file %s: %w", path, err)
	}

	buf := bytes.Buffer{}
	zw := gzip.NewWriter(&buf)
	if _, err := zw.Write(d); err != nil {
		return fmt.Errorf("failed to write zipped file %w", err)
	}
	if err := zw.Close(); err != nil {
		return fmt.Errorf("failed to close zipped file %w", err)
	}
	path, err = filepath.Rel(Config.OutDir, path)
	if err != nil {
		return err
	}
	return writeFile(path+".gz", buf.Bytes())
}

func compressTarGZ(root string, out string) error {
	outPath := out + ".tar.gz"
	f, err := os.Create(outPath)
	if err != nil {
		return fmt.Errorf("failed to create metrics tarball %s: %w", outPath, err)
	}
	defer f.Close()

	zw := gzip.NewWriter(f)
	tw := tar.NewWriter(zw)

	err = filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		var header *tar.Header

		if err != nil {
			return err
		}

		if filepath.Clean(path) == filepath.Clean(outPath) {
			return nil
		}

		if header, err = tar.FileInfoHeader(info, path); err != nil {
			return err
		}

		header.Name, _ = filepath.Rel(Config.OutDir, path)
		if err = tw.WriteHeader(header); err != nil {
			return err
		}

		if !info.IsDir() {
			content, err := os.Open(path)
			if err != nil {
				return err
			}
			defer content.Close()

			if _, err := io.Copy(tw, content); err != nil {
				return err
			}
		}

		return nil
	})

	if err != nil {
		return fmt.Errorf("failed to walk and tar.gz directory %s: %w", root, err)
	}

	if err := tw.Close(); err != nil {
		return err
	}
	if err := zw.Close(); err != nil {
		return err
	}
	return nil
}
