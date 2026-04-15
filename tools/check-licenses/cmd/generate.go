// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
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
)

type GenerateCommand struct {
	configFile           string
	fuchsiaDir           string
	buildDir             string
	outDir               string
	licensesOutDir       string
	gnPath               string
	genProjectFile       string
	genIntermediateFile  string
	checkURLs            bool
	overwriteReadmeFiles bool
	outputLicenseFile    bool
	runAnalysis          bool
	logLevel             int
}

func (*GenerateCommand) Name() string { return "generate" }
func (*GenerateCommand) Synopsis() string {
	return "Run the full compliance pipeline and generate reports."
}
func (*GenerateCommand) Usage() string {
	return `generate [options] [<gn_target>]:
  Traverses the repository, executes the Google License Classifier, and generates SPDX/NOTICE files.
`
}

func (p *GenerateCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.configFile, "config_file", "{FUCHSIA_DIR}/tools/check-licenses/cmd/_config.json", "Root config file path.")
	f.StringVar(&p.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
	f.StringVar(&p.buildDir, "build_dir", os.Getenv("FUCHSIA_BUILD_DIR"), "Location of GN build directory.")
	f.StringVar(&p.outDir, "out_dir", "/tmp/check-licenses", "Directory to write outputs to.")
	f.StringVar(&p.licensesOutDir, "licenses_out_dir", "", "Directory to write license text segments.")

	f.StringVar(&p.gnPath, "gn_path", "{FUCHSIA_DIR}/prebuilt/third_party/gn/{PLATFORM}/gn", "Path to GN executable. Required when gen_filter_target is specified.")
	f.StringVar(&p.genProjectFile, "gen_project_file", "{BUILD_DIR}/project.json", "Path to 'project.json' output file.")
	f.StringVar(&p.genIntermediateFile, "gen_intermediate_file", "", "Path to intermediate serialized gen struct.")

	f.BoolVar(&p.checkURLs, "check_urls", false, "Flag for enabling checks for license URLs.")
	f.BoolVar(&p.overwriteReadmeFiles, "overwrite_readme_files", false, "Flag for enabling README.fuchsia file overwites.")

	f.BoolVar(&p.outputLicenseFile, "output_license_file", true, "Flag for enabling template expansions.")
	f.BoolVar(&p.runAnalysis, "run_analysis", true, "Flag for enabling license analysis and 'result' package tests.")

	f.IntVar(&p.logLevel, "log_level", 2, "Log level. Set to 0 for no logs, 1 to log to a file, 2 to log to stdout.")
}

func (p *GenerateCommand) Execute(_ context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if err := p.executeImpl(f); err != nil {
		fmt.Fprintf(os.Stderr, "check-licenses generate: %s\nSee go/fuchsia-licenses-playbook for information on resolving common errors.\n", err)
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}

func (p *GenerateCommand) executeImpl(f *flag.FlagSet) error {
	var err error

	if err := p.setupLogging(); err != nil {
		return fmt.Errorf("Failed to setup logging: %w", err)
	}

	defer metrics.PhaseDuration.Track()()

	configVars := make(map[string]string)

	// fuchsiaDir
	if p.fuchsiaDir == "" {
		p.fuchsiaDir = "."
	}
	if p.fuchsiaDir, err = filepath.Abs(p.fuchsiaDir); err != nil {
		return fmt.Errorf("Failed to get absolute directory for fuchsiaDir %s: %w", p.fuchsiaDir, err)
	}
	configVars["{FUCHSIA_DIR}"] = p.fuchsiaDir

	// buildDir
	if len(p.buildDir) > 0 {
		if p.buildDir, err = filepath.Abs(p.buildDir); err != nil {
			return fmt.Errorf("Failed to get absolute directory for buildDir %s: %w", p.buildDir, err)
		}
	}
	configVars["{BUILD_DIR}"] = p.buildDir

	// outDir
	rootOutDir := p.outDir
	if len(p.outDir) > 0 {
		p.outDir, err = filepath.Abs(p.outDir)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for outDir %s: %w", p.outDir, err)
		}
		rootOutDir = p.outDir

		if _, err := os.Stat(p.outDir); os.IsNotExist(err) {
			err := os.MkdirAll(p.outDir, PERMISSIONS_ALLRW_OWNERX)
			if err != nil {
				return fmt.Errorf("Failed to create out directory [%s]: %w\n", p.outDir, err)
			}
		}
	}
	configVars["{OUT_DIR}"] = p.outDir
	configVars["{ROOT_OUT_DIR}"] = rootOutDir

	// licensesOutDir
	if p.licensesOutDir != "" {
		p.licensesOutDir, err = filepath.Abs(p.licensesOutDir)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for licensesOutDir %s: %w", p.licensesOutDir, err)
		}
		if _, err := os.Stat(p.licensesOutDir); os.IsNotExist(err) {
			err := os.MkdirAll(p.licensesOutDir, PERMISSIONS_ALLRW_OWNERX)
			if err != nil {
				return fmt.Errorf("Failed to create licenses out directory [%s]: %w\n", p.licensesOutDir, err)
			}
		}
	}
	configVars["{LICENSES_OUT_DIR}"] = p.licensesOutDir

	// gnPath
	platform := DEFAULT_PLATFORM
	if runtime.GOOS == "darwin" {
		platform = PLATFORM_MACOS
	}
	if len(p.gnPath) > 0 {
		p.gnPath = strings.ReplaceAll(p.gnPath, "{FUCHSIA_DIR}", p.fuchsiaDir)
		p.gnPath = strings.ReplaceAll(p.gnPath, "{PLATFORM}", platform)
		p.gnPath, err = filepath.Abs(p.gnPath)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for gnPath %s: %w", p.gnPath, err)
		}
	}

	if len(p.genIntermediateFile) > 0 {
		p.genIntermediateFile = strings.ReplaceAll(p.genIntermediateFile, "{BUILD_DIR}", p.buildDir)
		p.genIntermediateFile, err = filepath.Abs(p.genIntermediateFile)
		if err != nil {
			return fmt.Errorf("Failed to get absolute directory for genIntermediateFile %s: %w", p.genIntermediateFile, err)
		}
	}
	p.genProjectFile = strings.ReplaceAll(p.genProjectFile, "{BUILD_DIR}", p.buildDir)
	p.genProjectFile, err = filepath.Abs(p.genProjectFile)
	if err != nil {
		return fmt.Errorf("Failed to get absolute directory for genProjectFile %s: %w", p.genProjectFile, err)
	}

	configVars["{GN_PATH}"] = p.gnPath
	configVars["{GEN_INTERMEDIATE_FILE}"] = p.genIntermediateFile
	configVars["{GEN_PROJECT_FILE}"] = p.genProjectFile
	configVars["{OUTPUT_LICENSE_FILE}"] = strconv.FormatBool(p.outputLicenseFile)
	configVars["{RUN_ANALYSIS}"] = strconv.FormatBool(p.runAnalysis)

	configVars["{CHECK_URLS}"] = strconv.FormatBool(p.checkURLs)
	configVars["{OVERWRITE_README_FILES}"] = strconv.FormatBool(p.overwriteReadmeFiles)

	// target
	target := defaultTarget
	if f.NArg() > 1 {
		return fmt.Errorf("check-licenses takes a maximum of 1 positional argument (filepath or gn target), got %v\n", f.NArg())
	}
	if f.NArg() == 1 {
		target = f.Arg(0)
	}
	configVars["{TARGET}"] = target

	spdxDocName := "Fuchsia"
	if target != defaultTarget {
		spdxDocName = target
	}
	configVars["{SPDX_DOC_NAME}"] = spdxDocName

	// configFile
	p.configFile = strings.ReplaceAll(p.configFile, "{FUCHSIA_DIR}", p.fuchsiaDir)
	Config, err = NewCheckLicensesConfig(p.configFile, configVars)
	if err != nil {
		return err
	}

	if err := os.Chdir(p.fuchsiaDir); err != nil {
		return err
	}

	if err := p.executePipeline(); err != nil {
		return fmt.Errorf("failed to analyze the given directory: %w", err)
	}
	return nil
}

func (p *GenerateCommand) setupLogging() error {
	// Log Level setup
	if w, err := getLogWriters(p.logLevel, p.outDir); err != nil {
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

// executePipeline kicks-off the check-licenses runthrough.
// It is assumed that all configuration settings have been set before this is called.
func (p *GenerateCommand) executePipeline() error {
	endTrack := metrics.TotalRuntime.Track()

	// Initialize all package configs.
	startInitialize := time.Now()
	log.Print("Initializing... ")
	if err := p.initialize(); err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startInitialize))

	// Traverse the repository, generating a tree of Directory and File objects in memory.
	startDirectory := time.Now()
	log.Print("Discovering files and folders... ")
	_, err := directory.NewDirectory(".", nil)
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startDirectory))

	// If we plan on generating an output notice file:
	// Filter out the projects that we don't care about (absent from the build graph).
	if Config.OutputLicenseFile {
		startFilter := time.Now()
		target := Config.Target
		if target == "" {
			target = "//:default"
		}
		log.Printf("Filtering out projects that are not in the build graph for [%s]...",
			target)
		if err := project.FilterProjects(); err != nil {
			return err
		}
		log.Printf("Done. [%v]\n", time.Since(startFilter))
	} else {
		for _, proj := range project.GetAllProjects() {
			project.AddFilteredProject(proj)
		}
		project.RootProject, _ = project.GetProject(".")
	}

	// License analysis happens in CQ.
	// There is no need to analyze them if all we want to do is produce a NOTICE file.
	if Config.RunAnalysis {
		// Analyze the remaining projects, and keep track of all found license texts.
		startAnalyze := time.Now()
		log.Printf("Searching for license texts [%v projects]... ", len(project.GetAllFilteredProjects()))
		err = project.AnalyzeLicenses()
		if err != nil {
			return err
		}
		log.Printf("Done. [%v]\n", time.Since(startAnalyze))
	}

	// Save the resulting NOTICE file (if necessary), all config files
	// and execution metrics to the output directory.
	// Also perform checks to ensure the repository is in a good state.
	startSaveResults := time.Now()
	log.Print("Saving results... ")
	err = result.SaveResults()
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startSaveResults))

	// Done.
	endTrack() // Capture total execution time before printing summary

	// Print standard terminal metrics summary
	log.Println("\n[check-licenses] Execution Summary")
	log.Println("----------------------------------")
	log.Printf("Total Wall Time:                  %v\n", metrics.TotalRuntime.GetTotalDuration())
	log.Printf("Time spent in GN Filter:          %v\n", metrics.FilterDuration.GetTotalDuration())
	log.Printf("Wall time spent in Classifier:    %v\n", metrics.AnalyzeDuration.GetTotalDuration())
	log.Printf("Thread time spent in Classifier:  %v\n", metrics.ClassifierDuration.GetTotalDuration())
	projectsAnalyzed, err := metrics.ProjectsProcessed.GetCount("analyzed")
	if err != nil {
		return err
	}
	log.Printf("Projects Analyzed:         %d\n", projectsAnalyzed)

	rawTexts, err := metrics.LicenseDeduplication.GetCount("raw_texts")
	if err != nil {
		return err
	}
	uniqueTexts, err := metrics.LicenseDeduplication.GetCount("unique_texts")
	if err != nil {
		return err
	}
	compression := 0.0
	if rawTexts > 0 {
		compression = float64(rawTexts-uniqueTexts) / float64(rawTexts) * 100.0
	}
	log.Printf("Licenses Deduplicated:     %.1f%% compression (%d raw -> %d unique)\n", compression, rawTexts, uniqueTexts)

	var validationErrors int64 = 0
	var allowlistHits int64 = 0

	for _, check := range Config.Result.Checks {
		vErr, err := metrics.ValidationErrors.GetCount(check.Name)
		if err != nil {
			return err
		}
		validationErrors += vErr

		aHits, err := metrics.AllowlistHits.GetCount(check.Name)
		if err != nil {
			return err
		}
		allowlistHits += aHits
	}

	log.Printf("Validation Errors:         %d (%d Hidden by Allowlist)\n", validationErrors, allowlistHits)

	if Config.OutDir != "" {
		metricsExportPath := filepath.Join(Config.OutDir, "metrics.json")
		if err := metrics.Export(metricsExportPath); err != nil {
			log.Printf("Failed to export metrics to JSON: %v\n", err)
		} else {
			log.Printf("\nExported full metrics to:  %s\n", metricsExportPath)
		}
	}

	return nil
}

// Initialize each go package with their updated config files.
func (p *GenerateCommand) initialize() error {
	if err := file.Initialize(Config.File); err != nil {
		return err
	}
	if err := readme.Initialize(); err != nil {
		return err
	}
	if err := project.Initialize(Config.Project); err != nil {
		return err
	}
	if err := directory.Initialize(Config.Directory); err != nil {
		return err
	}
	if err := result.Initialize(Config.Result); err != nil {
		return err
	}

	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(Config, "", "  "); err != nil {
		return err
	} else {
		metrics.AddArtifact("cmd/_config.json", b)
	}

	return nil
}
