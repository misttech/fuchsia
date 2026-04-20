# Debug Adapter Protocol (DAP) Library


This library provides a complete and extensible implementation of the [Debug
Adapter Protocol
(DAP)](https://microsoft.github.io/debug-adapter-protocol/specification) in
Python.

## Features

- **Protocol Models**: Full set of Python types representing DAP requests,
  responses, and events.
- **Client Implementation**: A `DapClient` that handles the message framing
  (Content-Length headers) and asynchronous request/response matching.
- **Extensible Architecture**: Easy to add vendor-specific extensions without
  modifying the core library.


## Client Usage Example

Here is a basic example of how to use the `DapClient` to connect to a debug
adapter and send an initialize request:

```python
import asyncio
from pydap.client import DapClient
from pydap.models import InitializeArguments


async def run():
    client = DapClient()

    # Connect to the debug adapter (e.g., via TCP)
    reader, writer = await asyncio.open_connection("127.0.0.1", 12345)

    # Send initialize request
    args = InitializeArguments(adapter_id="test")

    # We create a task to send the request
    # In a full implementation, you would also have a task reading from 'reader'
    response = await client.initialize(writer, args)
    print(f"Response: {response}")


asyncio.run(run())
```


## Extensibility

One of the key design goals of this library is extensibility. The base protocol
is often extended by specific debuggers to support unique features.

To extend the library:
1. Define your custom argument types as dataclasses.
2. Use the `DapClient::send_request` method directly with your custom command
   string and arguments.

### Example: Custom Profiling Extension

Suppose you are building a debug adapter that supports a custom profiling
feature not covered by the standard protocol. You can extend `DapClient` to
support this server feature by subclassing or adding helper methods:

1. **Define your custom arguments**:
   ```python
   from dataclasses import dataclass


   @dataclass
   class StartProfilingArguments:
       # Duration in milliseconds.
       duration: int
   ```

2. **Send the custom request**:
   ```python
   import asyncio
   from pydap.models import dataclass_to_dict


   class CustomDapClient(DapClient):

       async def start_profiling(
           self, writer: asyncio.StreamWriter, duration: int
       ):
           args = StartProfilingArguments(duration=duration)
           return await self.send_request(
               writer, "startProfiling", dataclass_to_dict(args)
           )
   ```
