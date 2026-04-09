// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"sync"
)

type CmdMetrics struct {
	mu     sync.RWMutex
	counts map[string]int      `json:"counts"`
	values map[string][]string `json:"values"`
	files  map[string][]byte   `json:"files"`
}

var Metrics *CmdMetrics

func init() {
	Metrics = &CmdMetrics{
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

func (m *CmdMetrics) Counts() map[string]int {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.counts
}

func (m *CmdMetrics) Values() map[string][]string {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.values
}

func (m *CmdMetrics) Files() map[string][]byte {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.files
}
