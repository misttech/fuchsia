// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
)

const (
	defaultTarget = "//:default"

	PERMISSIONS_ALLRW_OWNERX = 0755

	PLATFORM_LINUX   = "linux-x64"
	PLATFORM_MACOS   = "mac-x64"
	DEFAULT_PLATFORM = PLATFORM_LINUX
)

var (
	Config *CheckLicensesConfig

	target = "//:default"
)

var (
	configFile     = flag.String("config_file", "{FUCHSIA_DIR}/tools/check-licenses/cmd/_config.json", "Root config file path.")
	fuchsiaDir     = flag.String("fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
	buildDir       = flag.String("build_dir", os.Getenv("FUCHSIA_BUILD_DIR"), "Location of GN build directory.")
	outDir         = flag.String("out_dir", "/tmp/check-licenses", "Directory to write outputs to.")
	licensesOutDir = flag.String("licenses_out_dir", "", "Directory to write license text segments.")

	gnPath              = flag.String("gn_path", "{FUCHSIA_DIR}/prebuilt/third_party/gn/{PLATFORM}/gn", "Path to GN executable. Required when gen_filter_target is specified.")
	genProjectFile      = flag.String("gen_project_file", "{BUILD_DIR}/project.json", "Path to 'project.json' output file.")
	genIntermediateFile = flag.String("gen_intermediate_file", "", "Path to intermediate serialized gen struct.")

	checkURLs            = flag.Bool("check_urls", false, "Flag for enabling checks for license URLs.")
	overwriteReadmeFiles = flag.Bool("overwrite_readme_files", false, "Flag for enabling README.fuchsia file overwites.")

	outputLicenseFile = flag.Bool("output_license_file", true, "Flag for enabling template expansions.")
	runAnalysis       = flag.Bool("run_analysis", true, "Flag for enabling license analysis and 'result' package tests.")

	logLevel = flag.Int("log_level", 2, "Log level. Set to 0 for no logs, 1 to log to a file, 2 to log to stdout.")
)

func setupLogging() error {
	// Log Level setup
	if w, err := getLogWriters(*logLevel, *outDir); err != nil {
		return err
	} else {
		log.SetOutput(w)
	}

	// Remove timestamps from logs
	log.SetFlags(0)

	return nil
}

// Log == 0: discard all output
// Log == 1: save logs to the outDir folder
// Log == 2: save logs to the outDir folder AND print to stdout
func getLogWriters(logLevel int, outDir string) (io.Writer, error) {
	logTargets := []io.Writer{}

	switch logLevel {
	case 0: // Discard all non-error logs.
		logTargets = append(logTargets, io.Discard)
	case 1: // Write all logs to a log file.
		if outDir != "" {
			if _, err := os.Stat(outDir); os.IsNotExist(err) {
				err := os.MkdirAll(outDir, 0755)
				if err != nil {
					return nil, fmt.Errorf("Failed to create out directory [%v]: %v\n", outDir, err)
				}
			}
			logfilePath := filepath.Join(outDir, "logs")
			f, err := os.OpenFile(logfilePath, os.O_RDWR|os.O_CREATE|os.O_APPEND, 0666)
			if err != nil {
				return nil, fmt.Errorf("Failed to create log file [%v]: %v\n", logfilePath, err)
			}
			// NOTE: We cannot close the file here, because the logger needs
			// to hold it open for the duration of the application's runtime.
			logTargets = append(logTargets, f)
		}
	case 2: // Write all logs to a log file and stdout.
		logTargets = append(logTargets, os.Stdout)
	}

	w := io.MultiWriter(logTargets...)
	return w, nil
}

func mainImpl() error {
	var err error

	flag.Parse()

	if err := setupLogging(); err != nil {
		return fmt.Errorf("Failed to setup logging: %w", err)
	}

	defer metrics.PhaseDuration.Track()()

	configVars := make(map[string]string)

	// fuchsiaDir
	if *fuchsiaDir == "" {
		*fuchsiaDir = "."
	}
	if *fuchsiaDir, err = filepath.Abs(*fuchsiaDir); err != nil {
		return fmt.Errorf("Failed to get absolute directory for *fuchsiaDir %s: %w", *fuchsiaDir, err)
	}
	configVars["{FUCHSIA_DIR}"] = *fuchsiaDir

	// buildDir
	if len(*buildDir) > 0 {
		if *buildDir, err = filepath.Abs(*buildDir); err != nil {
			return fmt.Errorf("Failed to get absolute directory for *buildDir %s: %w", *buildDir, err)
		}
	}
	configVars["{BUILD_DIR}"] = *buildDir

	// outDir
	rootOutDir := *outDir
	if len(*outDir) > 0 {
		*outDir, err = filepath.Abs(*outDir)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for *outDir %s: %w", *outDir, err)
		}
		rootOutDir = *outDir

		if _, err := os.Stat(*outDir); os.IsNotExist(err) {
			err := os.MkdirAll(*outDir, PERMISSIONS_ALLRW_OWNERX)
			if err != nil {
				return fmt.Errorf("Failed to create out directory [%s]: %w\n", *outDir, err)
			}
		}
	}
	configVars["{OUT_DIR}"] = *outDir
	configVars["{ROOT_OUT_DIR}"] = rootOutDir

	// licensesOutDir
	if *licensesOutDir != "" {
		*licensesOutDir, err = filepath.Abs(*licensesOutDir)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for *licensesOutDir %s: %w", *licensesOutDir, err)
		}
		if _, err := os.Stat(*licensesOutDir); os.IsNotExist(err) {
			err := os.MkdirAll(*licensesOutDir, PERMISSIONS_ALLRW_OWNERX)
			if err != nil {
				return fmt.Errorf("Failed to create licenses out directory [%s]: %w\n", *licensesOutDir, err)
			}
		}
	}
	configVars["{LICENSES_OUT_DIR}"] = *licensesOutDir

	// gnPath
	platform := DEFAULT_PLATFORM
	if runtime.GOOS == "darwin" {
		platform = PLATFORM_MACOS
	}
	if len(*gnPath) > 0 {
		*gnPath = strings.ReplaceAll(*gnPath, "{FUCHSIA_DIR}", *fuchsiaDir)
		*gnPath = strings.ReplaceAll(*gnPath, "{PLATFORM}", platform)
		*gnPath, err = filepath.Abs(*gnPath)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for *gnPath %s: %w", *gnPath, err)
		}
	}

	if len(*genIntermediateFile) > 0 {
		*genIntermediateFile = strings.ReplaceAll(*genIntermediateFile, "{BUILD_DIR}", *buildDir)
		*genIntermediateFile, err = filepath.Abs(*genIntermediateFile)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for *genIntermediateFile %s: %w", *genIntermediateFile, err)
		}
	}
	*genProjectFile = strings.ReplaceAll(*genProjectFile, "{BUILD_DIR}", *buildDir)
	*genProjectFile, err = filepath.Abs(*genProjectFile)
	if err != nil {
		return fmt.Errorf("Failed to get absolute directory for *genProjectFile %s: %w", *genProjectFile, err)
	}

	configVars["{GN_PATH}"] = *gnPath
	configVars["{GEN_INTERMEDIATE_FILE}"] = *genIntermediateFile
	configVars["{GEN_PROJECT_FILE}"] = *genProjectFile
	configVars["{OUTPUT_LICENSE_FILE}"] = strconv.FormatBool(*outputLicenseFile)
	configVars["{RUN_ANALYSIS}"] = strconv.FormatBool(*runAnalysis)

	configVars["{CHECK_URLS}"] = strconv.FormatBool(*checkURLs)
	configVars["{OVERWRITE_README_FILES}"] = strconv.FormatBool(*overwriteReadmeFiles)

	// target
	if flag.NArg() > 1 {
		return fmt.Errorf("check-licenses takes a maximum of 1 positional argument (filepath or gn target), got %v\n", flag.NArg())
	}
	if flag.NArg() == 1 {
		target = flag.Arg(0)
	}
	configVars["{TARGET}"] = target

	spdxDocName := "Fuchsia"
	if target != defaultTarget {
		spdxDocName = target
	}
	configVars["{SPDX_DOC_NAME}"] = spdxDocName

	// configFile
	*configFile = strings.ReplaceAll(*configFile, "{FUCHSIA_DIR}", *fuchsiaDir)
	Config, err = NewCheckLicensesConfig(*configFile, configVars)
	if err != nil {
		return err
	}

	if err := os.Chdir(*fuchsiaDir); err != nil {
		return err
	}

	if err := Execute(); err != nil {
		return fmt.Errorf("failed to analyze the given directory: %w", err)
	}
	return nil
}

func main() {
	if err := mainImpl(); err != nil {
		fmt.Fprintf(os.Stderr, "check-licenses: %s\nSee go/fuchsia-licenses-playbook for information on resolving common errors.\n", err)
		os.Exit(1)
	}
}
