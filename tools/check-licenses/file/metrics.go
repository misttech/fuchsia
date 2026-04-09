// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"sync"
)

type FileMetrics struct {
	mu     sync.RWMutex
	counts map[string]int      `json:"counts"`
	values map[string][]string `json:"values"`
	files  map[string][]byte   `json:"files"`
}

const (
	RepeatedFileTraversal    = "Files that were accessed multiple times during traversal"
	NumFiles                 = "All Files"
	NumPotentialLicenseFiles = "Files that may have license information"
)

var Metrics *FileMetrics

func init() {
	Metrics = &FileMetrics{
		counts: make(map[string]int),
		values: make(map[string][]string),
		files:  make(map[string][]byte),
	}

}

func plus1(key string) {
	Metrics.mu.Lock()
	defer Metrics.mu.Unlock()
	Metrics.counts[key] = Metrics.counts[key] + 1
}

func plusVal(key string, val string) {
	Metrics.mu.Lock()
	defer Metrics.mu.Unlock()
	Metrics.counts[key] = Metrics.counts[key] + 1
	Metrics.values[key] = append(Metrics.values[key], val)
}

func plusFile(key string, content []byte) {
	Metrics.mu.Lock()
	defer Metrics.mu.Unlock()
	Metrics.files[key] = content
}

func (m *FileMetrics) Counts() map[string]int {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.counts
}

func (m *FileMetrics) Values() map[string][]string {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.values
}

func (m *FileMetrics) Files() map[string][]byte {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.files
}
