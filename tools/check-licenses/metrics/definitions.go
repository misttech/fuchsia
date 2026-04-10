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
)
