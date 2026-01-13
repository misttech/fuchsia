# block-relay

Block-relay is a simple component which forwards partitions from fshost to
the driver framework.  Its goal is to make partitions (which may be served by
non-driver components) available in the driver topology by creating Nodes for
these partitions.

The block-relay has two inputs:

- A `/block` directory capability which contains all partitions to be forwarded,
  and
- The `fuchsia.hardware.block.volume.Service` service capability representing
  the device which the partitions are contained in.

At this time, block-relay requires exactly one volume service instance to be
available to it.  In the future, if we wanted to support multiple block devices,
we would need to disambiguate which block device each partition is part of.

Upon startup, block-relay will enumerate `/block`, and for each entry, obtain a
Block protocol connection to that partition.  Then, block-relay will call
`fuchsia.hardware.block.volume.Node/AddChild`, which makes the partition visible
in the driver framework topology.
