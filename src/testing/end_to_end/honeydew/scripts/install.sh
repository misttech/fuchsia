#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Creates a new virtual environment and pip installs Honeydew

LACEWING_SRC="$FUCHSIA_DIR/src/testing/end_to_end"
HONEYDEW_SRC="$LACEWING_SRC/honeydew"
BUILD_DIR=$(cat "$FUCHSIA_DIR"/.fx-build-dir)
FASTBOOT_PATH="$FUCHSIA_DIR/prebuilt/third_party/fastboot/fastboot"

VENV_ROOT_PATH="$LACEWING_SRC/.venvs"
VENV_NAME="fuchsia_python_venv"
VENV_PATH="$VENV_ROOT_PATH/$VENV_NAME"

# https://stackoverflow.com/questions/1871549/determine-if-python-is-running-inside-virtualenv
INSIDE_VENV=$(fuchsia-vendored-python -c 'import sys; print ("0" if (sys.base_prefix == sys.prefix) else "1")')
if [[ "$INSIDE_VENV" == "1" ]]; then
    echo
    echo "ERROR: Inside a virtual environment. Deactivate it and then run this script..."
    echo
    exit 1
fi

# Create a virtual environment using `fuchsia-vendored-python`
STARTING_DIR=`pwd`
mkdir -p $VENV_ROOT_PATH

if [ -d $VENV_PATH ]
then
    echo "Directory '$VENV_PATH' already exists. Deleting it..."
    rm -rf $VENV_PATH
fi
echo "Creating a new virtual environment @ '$VENV_PATH'..."
fuchsia-vendored-python -m venv $VENV_PATH

# activate the virtual environment
echo "Activating the virtual environment..."
source $VENV_PATH/bin/activate

# upgrade the `pip` module
echo "Upgrading pip module..."
python -m pip install --upgrade pip

# install Honeydew
echo "Installing 'Honeydew' module..."
cd $HONEYDEW_SRC
python -m pip install --editable ".[test,guidelines]"

echo "Configuring environment for Honeydew..."
NEW_PATH=$($FUCHSIA_DIR/src/testing/end_to_end/honeydew/scripts/conformance_paths.py --python-path-json "$FUCHSIA_DIR/$BUILD_DIR/fidl_python_dirs.json" --fuchsia-dir "$FUCHSIA_DIR" --build-dir "$FUCHSIA_DIR/$BUILD_DIR")
if [[ $? -ne 0 ]]; then
    echo "Failed to get PYTHONPATH"
    echo "$NEW_PATH"
    exit 1
fi
OLD_PYTHONPATH=$PYTHONPATH
PYTHONPATH=$NEW_PATH:$PYTHONPATH

export HONEYDEW_FASTBOOT_OVERRIDE=$FASTBOOT_PATH

python -c "import honeydew"
if [ $? -eq 0 ]; then
    echo "Successfully installed Honeydew"
else
    echo
    echo "ERROR: Honeydew installation failed. Please try again by following instructions manually"
    echo
    exit 1
fi

echo "Restoring environment..."
HD_PYTHONPATH=$PYTHONPATH
PYTHONPATH=$OLD_PYTHONPATH

cd $STARTING_DIR

echo -e "Installation successful...\n"
echo "To experiment with Honeydew locally in a Python interpreter, run:"
echo "  source $VENV_PATH/bin/activate &&"
echo "  export HONEYDEW_FASTBOOT_OVERRIDE=$FASTBOOT_PATH &&"
echo "  PYTHONPATH=$HD_PYTHONPATH python"
echo -e ">>> import honeydew\n"
