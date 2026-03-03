# Diagnostics Persistence: Saving Inspect across reboot

Diagnostics Persistence is a service that stores specific Inspect data on device
across one or more reboots. If you need to track failure states, historical
metrics, or other telemetry that must survive a restart, configure this service
to save your data automatically.

## Behavior and use cases

Persistence is particularly useful for recovering diagnostic data after:

- **Unexpected reboots:** Persistence saves data periodically based on your
  configured frequency and survives device restarts.
- **Component crashes:** Persistence never drops a value once it has been
  saved. For example, if your component publishes data in the first sample, but
  drops that data or crashes prior to the next sample, Persistence retains the
  original data in the persistence file.

## Quickstart guide

To configure Persistence to save your component's Inspect data, follow these
steps:

- [Identify your Inspect data](#identify-your-inspect-data)
- [Create a configuration file](#create-a-configuration-file)
- [Estimate `max_bytes`](#estimate-max_bytes)
- [Update the build](#update-the-build)

### 1. Identify your Inspect data {#identify-your-inspect-data}

Select the specific data you need to save. You will need the exact `INSPECT:`
selector for your component's data
(e.g., `INSPECT:core/pkg-resolver:root/resolver_service/active_package_resolves:*`).

### 2. Create a configuration file {#create-a-configuration-file}

Create a new `.persist` file in either `//src/diagnostics/config/persistence`
or `//vendor/*/diagnostics/config/persistence`.

The file uses JSON5. Define the parameters for the data you wish to persist:

```json5
[
  {
    tag: "cache-fallbacks", // Unique name
    service_name: "pkg-resolver", // Grouping for tags
    max_bytes: 500, // Max size of the persisted data
    min_seconds_between_fetch: 3600, // How frequently to sample the data
    selectors: [
      "INSPECT:core/pkg-resolver:root/resolver_service:cache_fallbacks_due_to_not_found",
      "INSPECT:core/pkg-resolver:root/resolver_service/active_package_resolves:*",
    ],
  },
]
```

### 3. Estimate `max_bytes` {#estimate-max_bytes}

The size of the persisted data is enforced at runtime. If your selectors fetch
more data than `max_bytes`, all of the saved data for this tag will be
permanently dropped and replaced with a single error string instead.

To estimate the correct `max_bytes` limit:

1.  Run your component on a device and populate the Inspect data you wish to
    persist. For more information on populating Inspect data, see
    [Codelab: Using Inspect](/docs/development/diagnostics/inspect/codelab.md).
2.  Run `ffx inspect show` locally with your exact selectors, and pipe the
    output through `jq` to strip away un-persisted data. Finally, count the bytes
    using `wc -c`. For example:

```bash
ffx --machine json inspect show \
    'core/pkg-resolver:root/resolver_service:cache_fallbacks_due_to_not_found' \
    'core/pkg-resolver:root/resolver_service/active_package_resolves:*' \
    | jq -c '.[] | pick(.moniker, .payload.root)' \
    | wc -c
```

3.  Add a generous buffer (e.g., 20-50%) to this total to account for string
    length variations, future field additions, and JSON formatting overhead.

### 4. Update the build {#update-the-build}

Add your new configuration file to the build by adding it to the `diagnostics-persistence`
package configuration in `//bundles/assembly/BUILD.gn`.

Find the `package_name = "diagnostics-persistence"` block and add your `.persist` file
to the `files` list:

```gn {:.devsite-disable-click-to-copy}
      package_name = "diagnostics-persistence"
      files = [
        {
          source = "//src/diagnostics/config/persistence/netstack.persist"
          destination = "netstack.persist"
        },
+       {
+         source = "//src/sys/pkg/bin/pkg-resolver/pkg-resolver.persist"
+         destination = "pkg-resolver.persist"
+       },
      ]
```

## Reading persisted data

On the next boot (after the software update check completes), the saved data is
re-published into Inspect.

The data is hosted by the `diagnostics-persistence` component. The original path
is prefixed with your configured `service_name` and `tag`.

```bash
$ ffx inspect show core/diagnostics/persistence
core/diagnostics/persistence:
  root:
    persist:
      pkg-resolver:
        cache-fallbacks:
          core/pkg-resolver:
            resolver_service:
              cache_fallbacks_due_to_not_found: 2
```

## Configuration reference

The `.persist` JSON5 file format expects an array of objects, where each object
defines a Persistence Tag. Each tag accepts the following fields:

| Field                           | Type                      | Description                                                                                                                                                     |
| ------------------------------- | ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **`tag`**                       | string                    | The unique identifier for this data collection within the `service_name`. Must be lowercase and hyphens only (e.g., `"my-feature-stats"`).                      |
| **`service_name`**              | string                    | A grouping identifier for related tags. Must be lowercase and hyphens only (e.g., `"my-service"`).                                                              |
| **`selectors`**                 | []string                  | A list of exact `INSPECT:` selectors to harvest and save.                                                                                                       |
| **`max_bytes`**                 | integer                   | The maximum allowed size of the fetched Inspect payload in bytes. If the sampled data exceeds this limit, the saved data will be replaced with an error string. |
| **`min_seconds_between_fetch`** | integer                   | How frequently Archivist should sample these selectors.                                                                                                         |
| **`persist_across_boot`**       | boolean (default `false`) | If `true`, saved data is not cleared on the next boot and will continue to accumulate historical boot data.                                                     |

## Privacy considerations

Persistence is a powerful tool for debugging, but it can also be a privacy risk
if not used carefully.

- **Cross-boot linkage**: Enabling `persist_across_boot` preserves saved data
  across boots, accumulating historical data. This creates a long-term record of
  device usage, which could be used to track users across multiple boots. This
  can also violate other privacy safeguards, such as fingerprinting a device or
  user across a time-limited pseudonymous ID.

- **Data retention**: Persistence data is stored on the device and can be
  accessed by anyone with physical access to the device. It is important to
  consider the sensitivity of the data you are persisting and whether it should
  be protected with additional security measures.

- **Data minimization**: Only persist the data that you need to debug your
  component. Avoid persisting unnecessary data, as this can increase the privacy
  risk.

## FAQ

### Does Persistence work with Lazy Nodes?

Yes. Persistence provides built-in support for Inspect [Lazy Nodes][lazy-nodes-link].
Persistence registers its required selectors and fetch frequencies with the
Archivist. At each interval, the Archivist actively queries the component's
selectors, which triggers the evaluation of any Lazy Nodes, allowing the system
to save their dynamically generated data.

[lazy-nodes-link]: /docs/development/diagnostics/inspect/quickstart.md#dynamic-values
