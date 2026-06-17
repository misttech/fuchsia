// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

// Readme represents a parsed README.fuchsia file.
type Readme struct {
	Name                     string `readme:"Name"`
	URL                      string `readme:"URL"`
	OriginalURL              string `readme:"Original URL"`
	CPEPrefix                string `readme:"CPEPrefix"`
	Version                  string `readme:"Version"`
	UpstreamGit              string `readme:"Upstream Git"`
	Revision                 string `readme:"Revision"`
	UpstreamRevision         string `readme:"Upstream Revision,Upstream revision"`
	SecurityCritical         string `readme:"Security Critical"`
	LicenseAndroidCompatible string `readme:"License Android Compatible"`
	Location                 string `readme:"Location"`

	Licenses        []string `readme:"License" separator:","`
	LicenseFiles    []string `readme:"License File" separator:","`
	SourceFiles     []string `readme:"Source File" separator:","`
	NonLicenseFiles []string `readme:"Non-License File" separator:","`

	UnknownFields []UnknownField `readme:"-"`

	Description        string `readme:"Description" multiline:"true"`
	LocalModifications string `readme:"Local Modifications,Modifications" multiline:"true"`
	Deprecated         string `readme:"Deprecated" multiline:"true"`
}

// UnknownField represents an unrecognized Key: Value pair found in the README.
type UnknownField struct {
	Key   string
	Value string
}
