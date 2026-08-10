# Reverser Component

This component provides a service that reverses strings. It explores
structured configuration to conditionally modify behavior. It is
primarily used to demonstrate dynamic configurations and test mocking
capabilities in Fuchsia.

## Structured Configuration

This component uses Structured Configuration to conditionally alter
its behavior at runtime. You can think of these config values as
analogous to command-line arguments or function parameters, provided
by the runner environment when the component starts.

### `switch_case`
- **Type**: `bool`
- **Description**: When set to `true`, the component will invert the
  alphabetical case of all characters in the string *in addition* to
  reversing the string order. When `false`, it only reverses the
  string.
- **Example Use Case**:
  - `switch_case: false` -> `"Hello Fuchsia!"` becomes `"!aishcuF olleH"`
  - `switch_case: true`  -> `"Hello Fuchsia!"` becomes `"!AISHCUf OLLEh"`
