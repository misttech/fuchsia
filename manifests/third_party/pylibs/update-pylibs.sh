#!/bin/bash
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# This script streamlines Python dependency management and project setup. It
# efficiently downloads all dependencies and their transitive requirements using
# pip, automatically extracts their contents, and populates a pylibs config file

set -eu -o pipefail

RED="$(tput setaf 1)"
NORM="$(tput sgr0)"

src_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly src_dir

download_dir="$(mktemp -d)"
readonly download_dir
trap 'rm -rf "${download_dir}"' EXIT

pkgs_dir="$(mktemp -d)"
readonly pkgs_dir
trap 'rm -rf "${pkgs_dir}"' EXIT

# TODO(maheshsr): don't hardcode linux-x64
python_dir="${FUCHSIA_DIR}/prebuilt/third_party/python3/linux-x64/bin"
readonly python_dir

# Make sure pip is up-to-date, otherwise some packages fail to install.
"${python_dir}/python3" -m pip install --quiet --upgrade pip
"${python_dir}/python3" -m pip install --upgrade setuptools

"${python_dir}/python3" -m pip download \
  -r "${src_dir}/requirements.txt" \
  --no-binary ":all:" \
  --dest "${download_dir}"

configfile="${src_dir}/pylibs"
readonly configfile
echo -n "" >"${configfile}"

cat >>"${configfile}" <<-EOF
<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <!--
  Configuration of python packages. This file was automatically generated by the
  script //pylibs/update-pylibs.sh.

  To add/update a Python package:
    * Edit the //pylibs/requirements.txt file with desired version.
    * Rerun the //pylibs/update-pylibs.sh script.
  -->
  <projects>
EOF

shopt -s nullglob

# Store filenames in array in sorted order
sorted_archives=($(ls -1 "${download_dir}"/*.tar.gz | sort))
readonly sorted_archives

for archive in "${sorted_archives[@]}"; do
  echo " Adding package to config file: $(basename "${archive}")"
  base="$(basename "${archive}")"
  noext="${base%.tar.gz}"

  # Extracts the package name from a base directory, removing trailing version number i.e mypy-1.6.0.
  pkg="$(sed -r 's/-[0-9]+\.[0-9]+(\.[0-9]+)?//g' <<<"${noext}")"

  # Extracts the version number (major.minor(.patch)) from the base directory,
  # handling both "major.minor.patch" and "major.minor" patterns i.e mypy-1.6.0, mypy-0.1
  version="$(sed -r 's/^.*-([0-9]+\.[0-9]+(\.[0-9]+)?).*/\1/g' <<<"${noext}")"

  unzip_dir="$(mktemp -d)"
  tar -xzf "${archive}" -C "${unzip_dir}"

  dest_dir="${pkgs_dir}/${pkg}"
  rm -rf "${dest_dir}"
  mv "${unzip_dir}/${noext}" "${dest_dir}"

  package_exists=false
  is_ignore=false
  # verifies the py package presence in a requirements.txt file and fetch the package url
  while read -r line; do
    # Checks whether line is empty or comment line
    if [[ ! "$line" =~ "^[[:space:]]*#" && -n "$line" ]]; then
      pkg_name="${line%% *}"     # Extract package name
      pkg_name="${pkg_name%%=*}" # Remove any trailing '==' and version

      if [[ "${pkg_name}" == "${pkg}" ]]; then
        package_exists=true
        sub_path="${line#*# }"
        path="third_party/${sub_path}"

        if [[ ${sub_path^^}  == "IGNORE" ]] ; then
            is_ignore=true
        fi
        break
      fi
    fi
  done <"${src_dir}/requirements.txt"

  if ! "${package_exists}"; then
    echo -e "${RED}Error: Add ${pkg} package name and path in the requirements.txt.${NORM}"
    exit
  fi

  # Ignore packages with ignore tag in the requirements.txt file
  if "${is_ignore}"; then
    echo "Ignoring ${pkg} package in the config file."
    continue
  fi

  all_tags=$(git ls-remote --tags "https://fuchsia-review.googlesource.com/${path}")
  if [[ -z "$all_tags" ]]; then
    echo -e "${RED}Error: No tags found for $noext.${NORM}"
    exit
  fi

  # Extracts the commit ID from the version tag at HEAD (if present)
  # Matches "refs/tags/" followed by optional letters, hyphens, or underscores,
  # and then the version number at the end of the line.
  matching_tag=$(echo "$all_tags" |
    grep -P "refs/tags/[-_a-zA-Z]*${version}$" |
    head -n 1)
  if [[ -z "$matching_tag" ]]; then
    echo -e "${RED}Error: No matching tag found for $noext.${NORM}"
    exit
  fi

  cat >>"${configfile}" <<-EOF
    <!-- ${pkg}-${version} -->
    <project name="third_party/pylibs/${pkg}"
        path="third_party/pylibs/${pkg}/src"
        remote="https://fuchsia.googlesource.com/${path}"
        revision="$(echo "$matching_tag" | awk '{print $1}')"
        gerrithost="https://fuchsia-review.googlesource.com"/>
EOF
  rm -rf "${unzip_dir}"
done

cat >>"${configfile}" <<-EOF
  </projects>
</manifest>
EOF

shopt -u nullglob
