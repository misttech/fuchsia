// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"

	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/util"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	v2readme "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	v2boundary "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	v2classify "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	v2discover "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	v2prune "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	v2report "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	v2validate "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
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
	runV2                bool
	verifyReadmes        bool
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

	f.BoolVar(&p.runV2, "v2", false, "Run the experimental v2 pipeline architecture.")
	f.BoolVar(&p.verifyReadmes, "verify_readmes", false, "Flag for verifying if README.fuchsia files accurately reflect project licenses in v2 pipeline.")
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
	fuchsiaDir, _, err := ResolveAndValidatePath(p.fuchsiaDir, ".")
	if err != nil {
		return err
	}
	p.fuchsiaDir = fuchsiaDir
	configVars["{FUCHSIA_DIR}"] = fuchsiaDir

	// Helper to resolve and optionally create directories
	resolvePath := func(path string, mkdir bool) (string, error) {
		if path == "" {
			return "", nil
		}
		absPath := path
		if !filepath.IsAbs(path) {
			absPath = filepath.Join(fuchsiaDir, path)
		}
		absPath, err = filepath.Abs(absPath)
		if err != nil {
			return "", err
		}
		if mkdir {
			if _, err := os.Stat(absPath); os.IsNotExist(err) {
				if err := os.MkdirAll(absPath, 0755); err != nil {
					return "", err
				}
			}
		}
		return absPath, nil
	}

	// buildDir
	p.buildDir, err = resolvePath(p.buildDir, false)
	if err != nil {
		return fmt.Errorf("failed to resolve buildDir: %w", err)
	}
	configVars["{BUILD_DIR}"] = p.buildDir

	// outDir
	p.outDir, err = resolvePath(p.outDir, true)
	if err != nil {
		return fmt.Errorf("failed to resolve outDir: %w", err)
	}
	configVars["{OUT_DIR}"] = p.outDir
	configVars["{ROOT_OUT_DIR}"] = p.outDir

	// licensesOutDir
	p.licensesOutDir, err = resolvePath(p.licensesOutDir, true)
	if err != nil {
		return fmt.Errorf("failed to resolve licensesOutDir: %w", err)
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
		p.gnPath, err = resolvePath(p.gnPath, false)
		if err != nil {
			return fmt.Errorf("failed to resolve gnPath: %w", err)
		}
	}

	if len(p.genIntermediateFile) > 0 {
		p.genIntermediateFile = strings.ReplaceAll(p.genIntermediateFile, "{BUILD_DIR}", p.buildDir)
		p.genIntermediateFile, err = resolvePath(p.genIntermediateFile, false)
		if err != nil {
			return fmt.Errorf("failed to resolve genIntermediateFile: %w", err)
		}
	}
	p.genProjectFile = strings.ReplaceAll(p.genProjectFile, "{BUILD_DIR}", p.buildDir)
	p.genProjectFile, err = resolvePath(p.genProjectFile, false)
	if err != nil {
		return fmt.Errorf("failed to resolve genProjectFile: %w", err)
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

	if p.runV2 {
		if err := p.executeV2Pipeline(target); err != nil {
			return fmt.Errorf("failed to execute v2 pipeline: %w", err)
		}
		return nil
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

	if logLevel == 1 || logLevel == 2 {
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
	}

	switch logLevel {
	case 0: // Discard all non-error logs.
		logTargets = append(logTargets, io.Discard)
	case 2: // Also print to stdout
		logTargets = append(logTargets, os.Stdout)
	}

	w := io.MultiWriter(logTargets...)
	return w, nil
}

// executeV2Pipeline runs the experimental v2 compliance engine.
func (p *GenerateCommand) executeV2Pipeline(target string) error {
	log.Println("Starting v2 fast compliance pipeline...")
	startTime := time.Now()
	ctx := context.Background()

	endTrack := metrics.TotalRuntime.Track()

	// 1. Assembly Phase
	builder := v2config.NewBuilder(p.fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		return fmt.Errorf("failed to assemble configuration: %w", err)
	}

	config := builder.Config
	log.Printf("Assembled configuration in %v", time.Since(startTime))

	// We still use the GN parsing logic for now to establish the build graph map
	var validFiles map[string]bool
	if p.outputLicenseFile {
		gnStart := time.Now()
		log.Printf("Generating GN project file to extract build graph... (This may take a while)")

		gn, err := util.NewGn(p.gnPath, p.buildDir)
		if err != nil {
			return err
		}
		if err := gn.GenerateProjectFile(ctx); err != nil {
			return err
		}

		log.Printf("Loading and parsing GN project.json file...")
		gen, err := util.LoadGen(p.genProjectFile)
		if err != nil {
			return err
		}

		log.Printf("Extracting transitive files from build graph...")
		validFiles, err = gen.GetTransitiveFiles(target, p.fuchsiaDir)
		if err != nil {
			return err
		}
		log.Printf("Build graph resolution complete in %v (Found %d valid files)", time.Since(gnStart), len(validFiles))
	}

	// 3. Instantiate Stages
	discoverer := v2discover.NewCrawler(p.fuchsiaDir, config.SkipPaths, config.SkipAnywhere)

	// Pass true for filesInReadmeOnly to match current behavior!
	grouper := v2boundary.NewGrouper(p.fuchsiaDir, config.BarrierPaths, config.OutOfTreeReadmes, true)

	pruner := v2prune.NewPruner(validFiles)

	patternsDir := filepath.Join(p.fuchsiaDir, "tools", "check-licenses", "assets", "patterns")
	baseClassifier, err := v2classify.NewClassifier(0.8, []string{patternsDir}, config.TargetExtensions)
	if err != nil {
		return fmt.Errorf("failed to initialize classifier: %w", err)
	}

	// Wrap in CustomClassifier!
	classifier := &CustomClassifier{
		Base:       baseClassifier,
		FuchsiaDir: p.fuchsiaDir,
	}

	validator := v2validate.NewValidator(p.fuchsiaDir, config.PolicyExceptions, config.AllowedLicenses, config.CopyrightExtensions)

	// Reporter: p.overwriteReadmeFiles is passed!
	reporter := v2report.NewReporter(p.fuchsiaDir, p.outDir, false, p.overwriteReadmeFiles, true, config.OutOfTreeReadmes, config.PolicyExceptions[v2config.PolicyCheckAllProjectsMustHaveALicense])

	orchestrator := pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, reporter)

	if err := orchestrator.Run(ctx, []string{p.fuchsiaDir}); err != nil {
		return fmt.Errorf("pipeline execution failed: %w", err)
	}

	// Print errors collected by CustomClassifier
	classifier.PrintErrors()

	endTrack()

	log.Printf("v2 pipeline completed successfully in %v\n", time.Since(startTime))
	return printMetricsSummary(nil, true, p.logLevel, p.outDir)
}

func printMetricsSummary(checkNames []string, isV2 bool, logLevel int, outDir string) error {
	// Print standard terminal metrics summary
	log.Println("\n[check-licenses] Execution Summary")
	log.Println("----------------------------------")
	log.Printf("Total Wall Time:                  %v\n", metrics.TotalRuntime.GetTotalDuration())
	log.Printf("Time spent in GN Filter:          %v\n", metrics.FilterDuration.GetTotalDuration())
	log.Printf("Wall time spent in Classifier:    %v\n", metrics.AnalyzeDuration.GetTotalDuration())
	log.Printf("Thread time spent in Classifier:  %v\n", metrics.ClassifierDuration.GetTotalDuration())

	totalFiles, _ := metrics.TotalFilesProcessed.GetCount()
	licenseFiles, _ := metrics.LicenseFilesFound.GetCount()
	sourceFilesWithLic, _ := metrics.SourceFilesWithLicenses.GetCount()

	log.Printf("Total Files Processed:            %d\n", totalFiles)
	log.Printf("License Files Found:              %d\n", licenseFiles)
	log.Printf("Source Files with Licenses:       %d\n", sourceFilesWithLic)

	var projectsAnalyzed int64
	var err error
	if isV2 {
		projectsAnalyzed, err = metrics.ProjectsProcessed.GetCount("kept_by_gn")
	} else {
		projectsAnalyzed, err = metrics.ProjectsProcessed.GetCount("analyzed")
	}
	if err != nil {
		projectsAnalyzed = 0
	}
	log.Printf("Projects Analyzed:         %d\n", projectsAnalyzed)

	rawTexts, err := metrics.LicenseDeduplication.GetCount("raw_texts")
	if err != nil {
		rawTexts = 0
	}
	uniqueTexts, err := metrics.LicenseDeduplication.GetCount("unique_texts")
	if err != nil {
		uniqueTexts = 0
	}
	compression := 0.0
	if rawTexts > 0 {
		compression = float64(rawTexts-uniqueTexts) / float64(rawTexts) * 100.0
	}
	log.Printf("Licenses Deduplicated:     %.1f%% compression (%d raw -> %d unique)\n", compression, rawTexts, uniqueTexts)

	var validationErrors int64 = 0
	var allowlistHits int64 = 0

	for _, name := range checkNames {
		vErr, _ := metrics.ValidationErrors.GetCount(name)
		validationErrors += vErr

		aHits, _ := metrics.AllowlistHits.GetCount(name)
		allowlistHits += aHits
	}

	log.Printf("Validation Errors:         %d (%d Hidden by Allowlist)\n", validationErrors, allowlistHits)

	if outDir != "" {
		metricsExportPath := filepath.Join(outDir, "metrics.json")
		if err := metrics.Export(metricsExportPath); err != nil {
			log.Printf("Failed to export metrics to JSON: %v\n", err)
		} else {
			log.Printf("\nExported full metrics to:  %s\n", metricsExportPath)
		}
	}

	return nil
}

type CustomClassifier struct {
	Base       *v2classify.Classifier
	FuchsiaDir string
	Errors     []string
	mu         sync.Mutex
}

func (c *CustomClassifier) Run(ctx context.Context, in <-chan pipeline.FilteredProject) (<-chan pipeline.ClassifiedFile, error) {
	out := make(chan pipeline.ClassifiedFile)
	go func() {
		defer close(out)
		for proj := range in {
			// Read README.fuchsia to distinguish files
			readmePath := filepath.Join(proj.RootPath, "README.fuchsia")
			var readmes []*v2readme.Readme
			var err error
			if _, err := os.Stat(readmePath); err == nil {
				readmes, err = v2readme.ParseFile(readmePath)
			}

			licenseFiles := make(map[string]bool)
			sourceFiles := make(map[string]bool)
			if err == nil {
				for _, r := range readmes {
					for _, lf := range r.LicenseFiles {
						licenseFiles[filepath.Join(proj.RootPath, lf.Path)] = true
					}
					for _, sf := range r.SourceFiles {
						sourceFiles[filepath.Join(proj.RootPath, sf.Path)] = true
					}
				}
			}

			for _, f := range proj.Files {
				if licenseFiles[f.Path] {
					// Dedicated License File: copy verbatim!
					data, err := os.ReadFile(f.Path)
					if err != nil {
						log.Printf("Error reading license file %s: %v", f.Path, err)
						continue
					}
					// Find license type from README if possible
					licenseType := ""
					if err == nil {
						for _, r := range readmes {
							for _, lf := range r.LicenseFiles {
								if filepath.Join(proj.RootPath, lf.Path) == f.Path {
									licenseType = lf.License
									break
								}
							}
						}
					}

					matches := []pipeline.LicenseMatch{}
					if licenseType != "" {
						spdxIDs := strings.Split(licenseType, ",")
						for _, id := range spdxIDs {
							id = strings.TrimSpace(id)
							if id != "" {
								matches = append(matches, pipeline.LicenseMatch{
									SPDXID: id,
									Text:   data,
								})
							}
						}
					} else {
						// Fallback if no type in README
						matches = append(matches, pipeline.LicenseMatch{
							SPDXID: "Unknown",
							Text:   data,
						})
					}

					out <- pipeline.ClassifiedFile{
						Path:          f.Path,
						ProjectRoot:   proj.RootPath,
						IsLicenseFile: true,
						AnalyzedText:  data,
						Matches:       matches,
					}
				} else if sourceFiles[f.Path] {
					// Source File: must classify!
					isLicenseFile := v2classify.IsLicenseFilename(f.Path)
					// Find license type from README if possible
					licenseType := ""
					if err == nil {
						for _, r := range readmes {
							for _, sf := range r.SourceFiles {
								if filepath.Join(proj.RootPath, sf.Path) == f.Path {
									licenseType = sf.License
									break
								}
							}
						}
					}

					classified, err := c.Base.ClassifyFile(f.Path, proj.RootPath, isLicenseFile, licenseType)
					if err != nil {
						log.Printf("Error classifying source file %s: %v", f.Path, err)
						continue
					}

					if len(classified.Matches) == 0 {
						c.mu.Lock()
						c.Errors = append(c.Errors, fmt.Sprintf("❌ Error: Classifier could not detect a license in Source File: %s", f.Path))
						c.mu.Unlock()
						continue
					}
					out <- *classified
				} else {
					// Not in README, fallback to standard classification
					isLicenseFile := v2classify.IsLicenseFilename(f.Path)
					classified, err := c.Base.ClassifyFile(f.Path, proj.RootPath, isLicenseFile, "")
					if err == nil {
						out <- *classified
					}
				}
			}
		}
	}()
	return out, nil
}

func (c *CustomClassifier) PrintErrors() {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.Errors) > 0 {
		fmt.Fprintln(os.Stderr, "\n[CustomClassifier] Errors:")
		for _, err := range c.Errors {
			fmt.Fprintln(os.Stderr, err)
		}
	}
}
