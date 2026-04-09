// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"fmt"
	"hash/fnv"
	"strings"

	spdx_common "github.com/spdx/tools-golang/spdx/common"
	spdx "github.com/spdx/tools-golang/spdx/v2_2"
)

// Create an spdx.Package struct that matches the given project struct.
func (p *Project) GenerateSPDXPackage() (*spdx.Package, error) {
	h := fnv.New128a()
	h.Write(fmt.Appendf(nil, "%s %s", p.Root, p.Name))
	p.SPDXID = fmt.Sprintf("Package-%x", h.Sum([]byte{}))

	pkg := &spdx.Package{
		PackageName:                 p.Name,
		PackageSPDXIdentifier:       spdx_common.ElementID(p.SPDXID),
		PackageDownloadLocation:     "NOASSERTION",
		PackageVerificationCode:     spdx_common.PackageVerificationCode{Value: "0", ExcludedFiles: make([]string, 0)},
		PackageLicenseConcluded:     "NOASSERTION",
		PackageLicenseInfoFromFiles: []string{},
		PackageLicenseDeclared:      "NOASSERTION",
		PackageCopyrightText:        "NOASSERTION",

		// Initialize these fields to make the online validator happy.
		// https://tools.spdx.org/app/validate/
		PackageChecksums:          make([]spdx_common.Checksum, 0),
		Files:                     make([]*spdx.File, 0),
		FilesAnalyzed:             true,
		IsFilesAnalyzedTagPresent: false,
		IsUnpackaged:              false,
		Annotations:               make([]spdx.Annotation, 0),
	}

	// Some projects in the Fuchsia tree provide multiple license files.
	// Others have a single large notice file with multiple license texts.
	// In these cases, the SPDX project should specify the SPDX IDs of each
	// license file in a boolean format, e.g.:
	//
	//     (LicenseRef-A AND LicenseRef-B) OR LicenseRef-C
	//
	// More info: https://spdx.github.io/spdx-spec/SPDX-license-expressions/#d4-composite-license-expressions
	//
	// This section generates that statement by simply concatenating all
	// entries together with AND statements. License texts in the same file
	// will appear within the same parenthesis group.

	var fileStatements []string
	for _, l := range p.LicenseFiles {
		// Note: We cannot easily find upstream URL links for a single NOTICE file.
		if strings.Contains(p.Root, "prebuilt") {
			fileStatements = append(fileStatements, fmt.Sprintf("(%s)", l.SPDXID()))
		} else {
			ldata, err := l.Data()
			if err != nil {
				return nil, err
			}
			if len(ldata) == 0 {
				continue
			}

			var dataStatements []string
			for _, data := range ldata {
				dataStatements = append(dataStatements, data.SPDXID())
			}
			fileStatements = append(fileStatements, fmt.Sprintf("(%s)", strings.Join(dataStatements, " AND ")))
		}
	}

	if len(fileStatements) > 0 {
		pkg.PackageLicenseConcluded = strings.Join(fileStatements, " AND ")
	}

	p.Package = pkg
	return pkg, nil
}
