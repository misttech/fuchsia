// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package productbundle defines Go structs representing the schema of the
// product_bundle.json file produced by Fuchsia assembly.
//
// These structs are a subset of the ProductBundleV2 schema defined in Rust at
// //src/lib/assembly/product_bundle/src/v2.rs, containing only the fields
// required by host-side tools written in Go.
package productbundle

type BootloaderPartition struct {
	Type  string `json:"type"`
	Name  string `json:"name"`
	Image string `json:"image"`
}

type Partition struct {
	Type string `json:"type"`
	Name string `json:"name"`
	Slot string `json:"slot"`
}

type SystemImage struct {
	Type string `json:"type"`
	Name string `json:"name"`
	Path string `json:"path"`
}

type ProductBundle struct {
	Partitions struct {
		BootloaderPartitions []BootloaderPartition `json:"bootloader_partitions"`
		Partitions           []Partition           `json:"partitions"`
	} `json:"partitions"`
	SystemA []SystemImage `json:"system_a"`
}
