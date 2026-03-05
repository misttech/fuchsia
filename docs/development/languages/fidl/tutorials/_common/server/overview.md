keywords: event, tutorial, two-way method, Fuchsia, contentType:ApiReference, contentType:Concept, topic:Platform, FIDL, server, topic:FuchsiaSystems, fuchsia.examples.Echo, component, contentType:Workflow, protocol, topic:Build, fire and forget
description: This tutorial provides an overview of implementing and running a FIDL server for the fuchsia.examples.Echo protocol on Fuchsia, covering protocol implementation, package building, and serving.
<!-- These keywords are for search widget on fuchsia.dev. Do not remove. -->

This tutorial shows you how to implement a FIDL protocol
(`fuchsia.examples.Echo`) and run it on Fuchsia. This protocol has one method
of each kind: a fire and forget method, a two-way method, and an event:

```fidl
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/fidl/fuchsia.examples/echo.test.fidl" region_tag="echo" %}
```

For more on FIDL methods and messaging models, refer to the [FIDL concepts][concepts] page.

This document covers how to complete the following tasks:

* Implement a FIDL protocol.
* Build and run a package on Fuchsia.
* Serve a FIDL protocol.

The tutorial starts by creating a component that is served to a Fuchsia device
and run. Then, it gradually adds functionality to get the server up and running.

[concepts]: /docs/concepts/fidl/overview.md