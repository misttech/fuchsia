#!/usr/bin/env fish
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.'
#
# Script to migrate Antlion from //third_party to //src/testing/end_to_end.
#
# --- Configuration ---
# Assumes FUCHSIA_DIR is defined in your environment.
if not set -q FUCHSIA_DIR
    echo "ERROR: FUCHSIA_DIR is not set. Please set it to the root of your Fuchsia checkout."
    exit 1
end

# Paths relative to FUCHSIA_DIR
set OLD_PATH_REL third_party/antlion
set NEW_PATH_REL src/testing/end_to_end/antlion
set OLD_PATH_ABS "$FUCHSIA_DIR/$OLD_PATH_REL"
set NEW_PATH_ABS "$FUCHSIA_DIR/$NEW_PATH_REL"

function clean_ignored_files
    echo "--- Cleaning ignored files in Antlion ---"
    if test -d "$OLD_PATH_ABS"
        echo "Removing untracked and ignored files: git -C \"$OLD_PATH_ABS\" clean -xdf"
        git -C "$OLD_PATH_ABS" clean -xdf
        echo "Clean complete."
    else
        echo "WARN: Directory $OLD_PATH_ABS does not exist. Skipping clean."
    end
    echo "--- End Cleaning ignored files ---"
end

function copy_antlion
    echo "--- Copying Antlion ---"
    if not test -d "$NEW_PATH_ABS"
        echo "INFO: Destination directory $NEW_PATH_ABS does not exist. Creating it."
        mkdir -p "$NEW_PATH_ABS"
    end
    echo "Executing: cp -rn \"$OLD_PATH_ABS/.\" \"$NEW_PATH_ABS/\""
    cp -rn "$OLD_PATH_ABS/." "$NEW_PATH_ABS/"
    echo "Copy complete. Existing files were not overwritten."
    echo "--- End Copying Antlion ---"
end

function remove_history
    rm --verbose -rf "$NEW_PATH_ABS/.git"
    rm --verbose $NEW_PATH_ABS/CHANGELOG.md
end

set unneeded_files \
    .gitignore \
    .editorconfig \
    .git-blame-ignore-revs \
    pyproject.toml \
    format.sh \
    MANIFEST.in \
    setup.py \
    LICENSE

function remove_unneeded_files
    for config_file in $unneeded_files
        rm --verbose "$NEW_PATH_ABS/$config_file"
    end
end

function fix_copyright_comments
    for year in 2020 2021 2022 2023 2024 2025
        fastmod --fixed-strings --multiline --glob '*.py' '*.rs' -- "# Copyright $year The Fuchsia Authors
#
# Licensed under the Apache License, Version 2.0 (the \"License\");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an \"AS IS\" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License." '# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.' $NEW_PATH_ABS
    end

    # This list was manually determined by the result of the static-check
    # builder in CQ.
    set -l files_without_copyright \
        src/testing/end_to_end/antlion/packages/antlion/controllers/ap_lib/regulatory_channels.py \
        src/testing/end_to_end/antlion/packages/antlion/controllers/openwrt_lib/network_const.py \
        src/testing/end_to_end/antlion/packages/antlion/controllers/openwrt_lib/wireless_config.py \
        src/testing/end_to_end/antlion/packages/antlion/controllers/openwrt_lib/wireless_settings_applier.py \
        src/testing/end_to_end/antlion/packages/antlion/error.py \
        src/testing/end_to_end/antlion/runner/src/yaml.rs

    for file in $files_without_copyright
        set -l original
        read -z original <$file
        if string match -rq '\.py$' $file
            echo '# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
' >$file
        else if string match -rq '\.rs$' $file
            echo '// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
' >$file
        end
        echo $original >>$file
    end
end

function remove_nested_owners_files
    echo "--- Removing nested OWNERS files ---"
    echo "Executing: find \"$NEW_PATH_ABS\" -mindepth 2 -name \"OWNERS\" -exec rm -v {} +"
    find "$NEW_PATH_ABS" -mindepth 2 -name OWNERS -exec rm -v {} +
    echo "--- End Removing nested OWNERS files ---"
end

function discover_targets
    echo "--- Discovering Antlion-related targets ---"

    echo "INFO: This step finds all test targets that will be affected by the migration."

    set -l search_dirs \
        "$FUCHSIA_DIR/src/connectivity/wlan/tests/core" \
        "$FUCHSIA_DIR/src/connectivity/wlan/tests/wlanix" \
        "$FUCHSIA_DIR/third_party/antlion/tests"

    echo "INFO: Grepping for targets in the following directories:"
    for dir in $search_dirs
        echo "  - $dir"
    end

    set -l formatted_targets
    for build_file in (find $search_dirs -name BUILD.gn)
        set -l dir_path (dirname "$build_file" | string replace "$FUCHSIA_DIR/" "")
        # Grep for target definitions, ignoring lines that are commented out.
        # set -l targets (grep -oP '^\s*(?:group|[a-z_]*test[a-z_]*)\("[^\"]+' "$build_file" || true)
        set -l targets (string match -ra '^\s*\w+\("[^"]+' -- (cat -- "$build_file"))
        set -l targets (string match -rav '^import' $targets)
        for t in $targets
            set -l target_name (echo "$t" | string replace -r '^[^"]+"' '')
            set -l full_target_name "//$dir_path:$target_name"
            set -l new_full_target_name (echo "$full_target_name" | string replace "//third_party/antlion" "//src/testing/end_to_end/antlion")
            set -a formatted_targets "$new_full_target_name"
        end
    end

    echo ""
    echo "--- Discovered Targets ---"
    echo "The following targets have been identified:"
    for target in $formatted_targets
        echo "$target"
    end
    echo --------------------------
    echo ""
    echo "--- MANUAL ACTION REQUIRED ---"
    echo "To proceed, you must configure your build to include these test targets."
    echo "1. Set your build configuration:"
    echo "   fx set minimal.sorrel"
    echo ""
    echo "2. Open the build arguments editor:"
    echo "   fx args"
    echo ""
    echo "3. Add the following lines to the 'args.gn' file that opens:"
    echo "   # Added for Antlion migration"
    echo "host_labels = ["
    for target in $formatted_targets
        printf '  \"%s\",\n' "$target"
    end
    echo "]"
    echo ""
    echo "4. Save the file and exit the editor."
    echo "5. Run 'fx build' to ensure your configuration is correct before proceeding."
    echo ------------------------------
    echo ""
    echo "--- End Discovering Antlion-related targets ---"
end

function find_replace_third_party_antlion_in_build_files
    rg third_party/antlion --glob='!out/*' --glob='*.gn' --glob='*.gni' --files-with-matches -0 \
        | xargs -0 sed -i 's%third_party/antlion%src/testing/end_to_end/antlion%g'
end

# --- Main Execution ---
function main
    echo "Antlion Migration Script"
    echo ------------------------
    echo "This script is designed to be run step-by-step."
    echo "Call the functions below in the specified order."
    echo ------------------------
    echo "FUCHSIA_DIR: $FUCHSIA_DIR"
    echo "OLD_PATH_ABS: $OLD_PATH_ABS"
    echo "NEW_PATH_ABS: $NEW_PATH_ABS"
    echo ------------------------
    echo "Execution Order:"
    echo "  clean_ignored_files"
    echo "  copy_antlion"
    echo "  remove_history"
    echo "  remove_unneeded_files"
    echo "  remove_nested_owners_files"
    echo "  fix_copyright_comments"
    echo "  fx format-code"
    echo "  discover_targets"
    echo "  MANUAL STEP: Modify 'fx args' as instructed by the output of 'discover_targets'."
    echo "  MANUAL STEP: Run `fx build`."
    echo ------------------------
    echo "  MANUAL STEP: Delete //third_party/antlion"
    echo "  MANUAL STEP: Run `fx build` and expect build failures for missing //third_party/antlion."
    echo "  find_replace_third_party_antlion_in_build_files"
    echo "  MANUAL STEP: Commit change and run tryjobs."
    echo ------------------------
    echo "  search_remaining_third_party_antlion_usage"
    echo "  MANUAL STEP: Replace remaining usage of //third_party/antlion"
    echo "  MANUAL STEP: Commit change and run tryjobs."
    echo ------------------------

end

main

echo "Script loaded. Call functions manually to proceed."
