# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

FidlLibraryInfo = provider(
    "Contains information about a FIDL library",
    fields = {
        "info": "List of structs(name, files) representing the library's dependencies",
        "name": "Name of the FIDL library",
        "ir": "Path to the JSON file with the library's intermediate representation",
    },
)
