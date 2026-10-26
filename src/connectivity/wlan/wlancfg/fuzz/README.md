# WLAN Fuzz Test

## What This Fuzzer Tests

The fuzz test in this directory tests `SavedNetworksManager`. It exercises how the manager stores, looks up, and persists saved networks. The fuzzer mutates the SSID and credential inputs.

## Where to Find Fuzz Test Results

Run the fuzz test locally with `ffx fuzz`. Fuchsia fuzzers also run on [ClusterFuzz](https://google.github.io/clusterfuzz/), which files bugs for failing tests on [Buganizer](https://fxbug.dev).

For more details, see the [Fuchsia site fuzzing page](https://fuchsia.dev/fuchsia-src/development/testing/fuzzing/handle-results).

## Adding Tests

You can add more fuzz tests next to the existing test. Write a fuzz test like a unit test. Add the `#[fuzz]` attribute and pass the fuzzed variables as function arguments.

This fuzz test uses `NetworkIdentifier` and `Credential` as inputs because both types derive the [Arbitrary](https://docs.rs/arbitrary/0.4.0/arbitrary/trait.Arbitrary.html) trait. If a type does not implement `Arbitrary`, derive `Arbitrary` for it or construct the value from a byte buffer in the test.

For more information, read [the Fuchsia doc for writing a fuzzer](https://fuchsia.dev/fuchsia-src/development/testing/fuzzing/write-a-fuzzer#rust_1).
