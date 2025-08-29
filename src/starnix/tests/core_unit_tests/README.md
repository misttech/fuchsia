# starnix_core "unit" tests

This directory contains tests which previously were unit tests within
starnix_core but which caused cyclic dependencies as the Starnix kernel was
split into multiple crates.

Please do not add new tests here, instead write new syscall or kernel
integration tests.
