// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
)

// Validate checks if the README.fuchsia file structures contain all required fields
// and no unknown fields. Returns a slice of all encountered errors.
func Validate(readmes []*Readme) []error {
	var errs []error

	for i, r := range readmes {
		// Check 1: Unknown fields
		if len(r.UnknownFields) > 0 {
			errs = append(errs, fmt.Errorf("Readme %d: Found unknown/invalid fields: %+v", i+1, r.UnknownFields))
		}

		// Check 2: Required Fields
		if r.Name == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Name' is a required field", i+1))
		}
		if r.URL == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'URL' is a required field", i+1))
		}
		if r.Version == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Version' is a required field", i+1))
		}
		if r.SecurityCritical != "yes" && r.SecurityCritical != "no" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Security Critical' is required and must be exactly 'yes' or 'no'. Got: %q", i+1, r.SecurityCritical))
		}
		if i > 0 && r.Location == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Location' is a required field for sub-projects defined after a DEPENDENCY DIVIDER", i+1))
		}
		if len(r.LicenseFiles) == 0 {
			errs = append(errs, fmt.Errorf("Readme %d: At least one 'License File' must be specified", i+1))
		} else {
			for _, lf := range r.LicenseFiles {
				if lf.License == "" {
					errs = append(errs, fmt.Errorf("Readme %d: License File '%s' is missing required '  License:' metadata", i+1, lf.Path))
				}
			}
		}
	}

	return errs
}
