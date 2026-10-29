# Block device testing

Warning: All of the following tests are destructive, and they may not
ask for confirmation before executing. Run at your own risk.

## Protocol testing

`blktest` is an integration that may be used to check adherence to the block protocol.

```shell
$ blktest -d /block/000/fuchsia.storage.block.Block
```

## Filesystem testing

`fs-test` is a filesystem integration test suite that can be used to verify
Fuchsia filesystem correctness on a filesystem.

To avoid racing with the auto-mounter, it is recommended to run this
test with the assembly override:

```
--assembly-override '//products/core/*=//local:disable_automount'
```

with a `//local/BUILD.gn` file as follows:

```
import("//build/assembly/developer_overrides.gni")

assembly_developer_overrides("disable_automount") {
  platform = {
    storage = {
      disable_automount = false
    }
  }
}
```

```shell
$ /boot/test/fs/fs-test -d /block/000/fuchsia.storage.block.Block -f minfs
```

## Correctness testing

`iochk` is a tool that pseudorandomly reads and writes to a block device to check for errors.

```shell
$ iochk -bs 32k -t 8 /block/000/fuchsia.storage.block.Block
```

## Performance testing

`iotime` is a benchmarking tool that tests the read and write performance of block devices.

```shell
$ iotime read fifo /block/000/fuchsia.storage.block.Block 64m 4k
```


