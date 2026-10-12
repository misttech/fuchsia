# power-broker

Power Broker tracks the system's power elements and the dependencies between
their power levels. Clients request leases via `Topology.Lease` when they need
elements at higher levels, and Power Broker drives each element to the minimum
level needed to satisfy active leases through `ElementRunner.SetLevel`.

## Testing

Unit tests can be run with:

```sh
fx test power-broker-unittests
```

Integration tests can be run with:

```sh
fx test //src/power/broker/tests/integration
```
