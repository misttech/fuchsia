// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package metrics

var (
	FilesProcessed = RegisterCounter(
		"files_processed",
		"Number of files processed",
		"extension", "status", // status: "analyzed", "skipped", "cached", "error"
	)

	DiskReadDuration = RegisterTimer(
		"disk_read_duration",
		"Time spent reading files from disk",
	)

	ClassifierDuration = RegisterTimer(
		"classifier_duration",
		"Time spent executing the License Classifier",
	)

	NoticeParsingDuration = RegisterTimer(
		"notice_parsing_duration",
		"Time spent parsing and deduplicating upstream NOTICE files",
	)

	TransliterationDuration = RegisterTimer(
		"transliteration_duration",
		"Time spent transliterating legacy encodings to UTF-8",
	)

	FileTruncation = RegisterCounter(
		"file_truncation",
		"Number of regular files truncated vs read fully",
		"status", // status: "truncated", "full"
	)

	NoticeSegments = RegisterCounter(
		"notice_segments",
		"Number of raw segments found vs unique segments kept",
		"status", // status: "raw", "unique"
	)

	FileEncoding = RegisterCounter(
		"file_encoding",
		"Number of files with valid UTF-8 vs legacy encodings",
		"encoding", // encoding: "utf8", "legacy"
	)

	ReadmeParseDuration = RegisterTimer(
		"readme_parse_duration",
		"Time spent unmarshaling text proto files or Cargo.toml",
	)

	GitCommandDuration = RegisterTimer(
		"git_command_duration",
		"Time spent running git commands to discover upstream URLs",
	)

	MalformedReadmeLines = RegisterCounter(
		"malformed_readme_lines",
		"Number of lines in a README.fuchsia file that the parser could not understand",
		"type", // type: "unknown_directive", "parse_error"
	)

	DeprecatedDirectives = RegisterCounter(
		"deprecated_directives",
		"Usage of legacy syntax in README.fuchsia files",
		"directive",
	)

	ReadmeGenerationType = RegisterCounter(
		"readme_generation_type",
		"How a Readme struct was created",
		"type", // type: "explicit", "synthesized_rust", "synthesized_go", "synthesized_dart"
	)

	ReadmeCacheHits = RegisterCounter(
		"readme_cache_hits",
		"How often reading a README.fuchsia file was avoided because it was in memory",
		"status", // status: "hit"
	)

	DirectoryTraversalDuration = RegisterTimer(
		"directory_traversal_duration",
		"Time spent crawling the directory structure",
	)

	ReadmeParsingDuration = RegisterTimer(
		"readme_parsing_duration",
		"Time spent parsing README.fuchsia files",
	)

	BoundaryDetectionType = RegisterCounter(
		"boundary_detection_type",
		"How the tool decided to start a new project",
		"detection_type", // detection_type: "explicit_fuchsia", "explicit_chromium", "fallback_go", "fallback_rust", "fallback_dart", "inherited"
	)

	OrphanedEntities = RegisterCounter(
		"orphaned_entities",
		"Number of files or directories attributed to UnknownProject",
		"type", // type: "file", "directory"
	)

	DirectoriesProcessed = RegisterCounter(
		"directories_processed",
		"Number of directories processed",
		"status", // status: "analyzed", "skipped", "missing_project"
	)

	SymlinksProcessed = RegisterCounter(
		"symlinks_processed",
		"Number of symlinks processed",
		"status",
	)
)
