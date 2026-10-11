# FIDL tutorial

This guide explains how to export a FIDL service from a parent driver and
utilize it in a child driver. This guide assumes familiarity with the following
concepts:

*   [FIDL](/docs/development/languages/fidl/README.md)
*   [Driver binding](/docs/concepts/drivers/driver_binding.md)
*   [Write a minimal driver](/docs/development/drivers/developer_guide/write-a-minimal-dfv2-driver.md)
*   [Driver communication](/docs/concepts/drivers/driver_communication.md)
*   [C++ FIDL Bindings](/docs/development/languages/fidl/tutorials/cpp/README.md)

For a complete working example in the Fuchsia source tree, see the
[Zircon transport example](/examples/drivers/transport/zircon/v2/).

## FIDL Protocol Definition

The following snippets utilize this FIDL definition:

```fidl
library fidl.examples.echo;

const MAX_STRING_LENGTH uint64 = 32;

@discoverable
closed protocol Echo {
    /// Returns the input.
    strict EchoString(struct {
        value string:<MAX_STRING_LENGTH, optional>;
    }) -> (struct {
        response string:<MAX_STRING_LENGTH, optional>;
    });
};

service EchoService {
    echo client_end:Echo;
};
```

## Parent Driver (The Server)

The parent driver implements the `fidl.examples.echo/Echo` protocol, serves
`fidl.examples.echo.EchoService` in its outgoing directory, and offers that
service when adding a child node.

### Driver Manifest

In the parent driver's manifest (`.dml`), declare and expose the FIDL service
capability:

```json5
capabilities: [
    { service: "fidl.examples.echo.EchoService" },
],
expose: [
    {
        service: "fidl.examples.echo.EchoService",
        from: "self",
    },
],
```

### Server Code

```cpp
#include <fidl/fidl.examples.echo/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>

// This class implements the fidl.examples.echo/Echo FIDL protocol using the
// C++ Wire bindings.
class ParentDriver : public fdf::DriverBase2,
                     public fidl::WireServer<fidl_examples_echo::Echo> {
 public:
  ParentDriver() : fdf::DriverBase2("parent") {}

  zx::result<> Start(fdf::DriverContext context) override {
    // Add the FIDL service we wish to export to our child to our outgoing
    // directory. When a connection is attempted we bind the server end of
    // the channel pair to our server implementation using the driver's default
    // dispatcher.
    fidl_examples_echo::EchoService::InstanceHandler handler({
        .echo = bindings_.CreateHandler(this, dispatcher(),
                                        fidl::kIgnoreBindingClosure),
    });

    zx::result result =
        outgoing()->AddService<fidl_examples_echo::EchoService>(std::move(handler));
    if (result.is_error()) {
      fdf::error("Failed to add EchoService to outgoing directory: {}", result);
      return result.take_error();
    }

    // Declare our outgoing service offer and add the child node. The driver
    // framework uses this offer to route the service to the child and
    // automatically populate node properties for binding.
    std::vector<fuchsia_driver_framework::NodeProperty2> properties = {};
    zx::result child_result = AddChild(
        "echo_child", properties,
        std::array{fdf::MakeOffer2<fidl_examples_echo::EchoService>()});
    if (child_result.is_error()) {
      fdf::error("Failed to add child node: {}", child_result);
      return child_result.take_error();
    }
    controller_.Bind(std::move(child_result.value()), dispatcher());

    return zx::ok();
  }

 private:
  // Implementation of the method required by our FIDL protocol.
  void EchoString(EchoStringRequestView request,
                  EchoStringCompleter::Sync& completer) override {
    completer.Reply(request->value);
  }

  fidl::WireClient<fuchsia_driver_framework::NodeController> controller_;
  // Ensures that the FIDL connections do not outlive the driver instance.
  fidl::ServerBindingGroup<fidl_examples_echo::Echo> bindings_;
};

FUCHSIA_DRIVER_EXPORT2(ParentDriver);
```

## Child Driver (The Client)

### Driver Manifest

In the child driver's manifest (`.dml`), declare that the driver uses the
service:

```json5
use: [
    { service: "fidl.examples.echo.EchoService" },
],
```

Note that the child driver's bind rules are automatically generated from its
DML.

### Client Code

The following code snippet shows how a child driver that has bound to the parent
node connects to and calls the `Echo` protocol:

```cpp
#include <fidl/fidl.examples.echo/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>

class ChildDriver : public fdf::DriverBase2 {
 public:
  ChildDriver() : fdf::DriverBase2("child") {}

  zx::result<> Start(fdf::DriverContext context) override {
    // Connect to the protocol offered by our parent driver through our incoming
    // namespace. We do not need to specify the service or protocol name string
    // because Connect is templated on the generated service member type.
    zx::result client_end =
        context.incoming().Connect<fidl_examples_echo::EchoService::Echo>();
    if (client_end.is_error()) {
      fdf::error("Failed to connect to Echo protocol: {}", client_end);
      return client_end.take_error();
    }

    // Turn the client side of the endpoint pair into a synchronous client.
    fidl::WireSyncClient client{std::move(client_end.value())};

    // Utilize our client to make calls.
    constexpr std::string_view kInput = "Test String";

    fidl::WireResult result =
        client->EchoString(fidl::StringView::FromExternal(kInput));
    if (!result.ok()) {
      fdf::error("Failed to call EchoString: {}", result.error());
      return zx::error(result.status());
    }
    if (result->response.get() != kInput) {
      fdf::error("Unexpected response: Actual: \"{}\", Expected: \"{}\"",
                 result->response.get(), kInput);
      return zx::error(ZX_ERR_INTERNAL);
    }

    return zx::ok();
  }
};

FUCHSIA_DRIVER_EXPORT2(ChildDriver);
```
