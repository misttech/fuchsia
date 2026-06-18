#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.


# Formats the Honeydew code as per coding guidelines
LACEWING_SRC="$FUCHSIA_DIR/src/testing/end_to_end"
HONEYDEW_SRC="$LACEWING_SRC/honeydew"

VENV_ROOT_PATH="$LACEWING_SRC/.venvs"
VENV_NAME="fuchsia_python_venv"
VENV_PATH="$VENV_ROOT_PATH/$VENV_NAME"

if [ -d $VENV_PATH ]
then
    echo "Activating the virtual environment..."
    source $VENV_PATH/bin/activate
else
    echo
    echo "ERROR: Directory '$VENV_PATH' does not exists. Run the 'install.sh' script first..."
    echo
    exit 1
fi

cd $FUCHSIA_DIR

echo "Formatting the code..."
# Format the code (using black, isort and autoflake)
fx format-code

# To perform mypy checks, build honeydew target using `fx build`

echo "Running static type checking using 'ty'..."
BUILD_DIR=$(cat "$FUCHSIA_DIR/.fx-build-dir")
NEW_PATH=$($HONEYDEW_SRC/scripts/conformance_paths.py --python-path-json "$FUCHSIA_DIR/$BUILD_DIR/extra_python_dirs.json" --fuchsia-dir "$FUCHSIA_DIR" --build-dir "$FUCHSIA_DIR/$BUILD_DIR")

# Exclude functional tests from local 'ty' check because they depend on external
# libraries (like antlion) that might not be present in the local environment.
# These tests are still verified by 'mypy' in CQ where the full build graph is available.
PYTHONPATH="${NEW_PATH}${PYTHONPATH:+:${PYTHONPATH}}" ty check --exclude "**/functional_tests" $HONEYDEW_SRC/honeydew/ $HONEYDEW_SRC/tests/ || echo "WARNING: ty check failed. Codebase is transitioning from mypy to ty."

echo "Running static code analysis using 'pylint'..."
pylint --rcfile=$HONEYDEW_SRC/linter/pylintrc $HONEYDEW_SRC/honeydew/ > /dev/null 2>&1 \
&& \
pylint --rcfile=$HONEYDEW_SRC/linter/pylintrc $HONEYDEW_SRC/tests/ > /dev/null 2>&1
if [ $? -eq 0 ]; then
    echo "Code is 'pylint' compliant"
else
    echo
    echo "ERROR: Code is not 'pylint' compliant!"
    echo "ERROR: Please run below command sequence, fix all the issues and then rerun this script"
    echo "*************************************"
    echo "$ source $VENV_PATH/bin/activate"
    echo "$ pylint --rcfile=$HONEYDEW_SRC/linter/pylintrc $HONEYDEW_SRC/honeydew/"
    echo "$ pylint --rcfile=$HONEYDEW_SRC/linter/pylintrc $HONEYDEW_SRC/tests/"
    echo "*************************************"
    echo
    exit 1
fi

echo "Successfully completed all of the formatting checks"
