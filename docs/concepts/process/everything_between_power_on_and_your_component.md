# Everything between power on and your component

WARN: This document has not been reviewed recently and many details may be
stale.

This document aims to detail, at a high-level, everything that happens between
machine power on and software components running on the system.

Outline:

- [Kernel](#kernel)
- [Initial processes](#initial-processes)
- [Component manager](#component-manager)
- [Initial system components](#v2-components)
- [Legacy component framework](#v1-components)

## Kernel {#kernel}

The process for loading the Fuchsia kernel (zircon) onto the system varies by
platform. At a high level the kernel is stored in the
[ZBI][glossary.zircon boot image], which holds
everything needed to bootstrap Fuchsia.

[Once the kernel (zircon) is running on the system][bootloader-and-kernel] its
main objective is to start [userspace][userspace], where processes can be run.
Since zircon is like a [microkernel][micro-kernel], it doesn't have to do a whole
lot in this stage (especially compared to Linux). The executable for the first
user process is baked into the kernel, which the kernel copies into a new
process and starts. This program is called [userboot][userboot].

## Initial processes {#initial-processes}

Userboot is carefully constructed to be [easy for the kernel to
start][userboot-loading], because otherwise the kernel would have to implement a
lot of [process bootstrap functionality][process-bootstrap] (like a library
loader service) that would never be used after the first process has been
started.

Userboot’s job is really straightforward, to find and start the next process.
The kernel gives userboot a handle to the ZBI, inside of which is the
[bootfs][glossary.bootfs] image. Userboot reads through the ZBI to find the bootfs image,
decompresses it if necessary, and copies it to a fresh
[vmo][glossary.virtual memory object]. The bootfs
image contains a read-only filesystem, which userboot then accesses to find an
executable and its libraries. With these it starts the next process, which is
[component manager][component-manager].

Userboot may exit at this point, unless the userboot.shutdown option was given
on the [kernel command line][kernel-command-line].

Component manager, the next process, is [dynamically linked][dynamic-linking] by userboot. This
makes it a better home than userboot for early boot complex logic, as it can use
libraries. Because of this component manager runs various FIDL services for its children,
the most notable of which for boot purposes is bootfs, a [FIDL-based filesystem][fuchsia-io]
backed by the bootfs image that userboot decompressed. It also finishes parsing the ZBI and
decommits unnecessary pages, and uses the extracted information to host item, item factory, and
argument services.

Component manager marks its process as [critical][critical-processes], which means that if
something goes wrong and it crashes, the [job][job] that it is in is killed. As it runs in
the root job which has the special property that if it is killed the kernel force restarts the
system, component manager crashing will cause a reboot.

## Component manager {#component-manager}

[Component manager][component-manager] is the program that drives the
component framework. This framework controls how and when programs are run and
which capabilities these programs can access from other programs. A program run
by this framework is referred to as a [component][glossary.component].

The components that component manager runs are organized into a tree. There is a
root component, and it has two children named bootstrap and core. Bootstrap's
children are the parts of the system needed to get the system functional enough
to run higher-level software that includes the user experience.

The root, bootstrap, and core components are non-executable components, which
means that they have no program running on the system that corresponds to them.
They exists solely for organizational purposes.

![A diagram showing that fshost and driver manager, are children of the
bootstrap component and core and
bootstrap are children of the root component](images/v2-topology.png)

## Initial system components {#v2-components}

### Background

There are two important components under bootstrap, fshost and driver manager.
These two components work together to bring up a functional enough system for
the [session][glossary.session], which then starts up all the user-facing
software.

#### driver manager

[Driver manager][glossary.driver manager] is the component responsible for finding
hardware, running drivers to service the hardware, and exposing a handle for
[devfs][devfs] to Fuchsia. Devfs is being deprecated in favor of services,
which a driver exposes directly. (see [Driver Communication][driver-communication])

Drivers are run by [driver hosts][glossary.driver host], which are child processes that
driver manager starts. Each driver is a dynamic library stored in either bootfs
or a package, and when a driver is to be run it is dynamically linked into a
driver host and then executed.

The drivers stored in packages aren't available when driver manager starts, as
those are stored on disk and drivers must be running before block devices for
filesystems can appear. Before the filesystems are loaded, only drivers in the
Zircon Boot Image (ZBI) can be loaded and run. The Driver Index is a component
that knows where all of the drivers live in the system. The Driver Index will
let Driver Manager know when base packages have finished loading and it has
found base drivers.

#### fshost

Fshost is a component responsible for finding block devices, starting
filesystem processes to service these block devices, and
[providing handles][fshost-exposes] for these filesystems to the rest of
Fuchsia. To accomplish this, fshost attempts to access the /dev handle in its
namespace. This capability is
[provided by driver manager][driver-manager-exposes].

As fshost finds block devices, it
[reads headers from each device][fshost-magic-headers] to detect the filesystem
type. It will initially find the [Fuchsia Volume Manager][glossary.fuchsia-volume-manager]
(fvm) block, which points to partitions for other block devices. Fshost will
use devfs to cause driver manager to run the fvm driver for this block device,
which causes other block devices to appear for fshost to inspect. It does a
similar thing when it discovers a [zxcrypt][zxcrypt] partition, as the disk will
need to be decrypted to be usable. Once fvm and zxcrypt are loaded, fshost will
find the appropriate block devices and start the [minfs][minfs] and
[blobfs][blobfs] filesystems, which are needed for a fully functioning system.

### Startup sequence

Component manager generally starts components lazily on-demand in response to
something accessing a capability provided by the component. Components may also
be marked as "eager", which causes the component to start at the same point its
parent starts.

Once running, fshost attempts to access the `/dev` handle from driver manager,
which causes driver manager to start. Together they bring up drivers and
filesystems, eventually culminating in pkgfs running. At this point fshost
starts responding to requests on the `/pkgfs` handle, and component manager
proceeds to start the rest of userspace.

TODO(https://fxbug.dev/42053321): This diagram is no longer accurate as of
https://cs.opensource.google/fuchsia/fuchsia/+/124f955ae0d1db1c7e991684c7e8a9b4528d6806.

![A sequence diagram showing the startup sequence between fshost and driver
manager.](images/boot-sequence-diagram.png)

## Boot complete

At this point, the system is ready to launch additional components through FIDL
protocols and services, or by directly launching them with services provided by
component_manager.

[glossary.bootfs]: /docs/glossary#README.md#bootfs
[glossary.virtual memory object]: /docs/glossary#README.md#virtual-memory-object
[glossary.zircon boot image]: /docs/glossary#README.md#zircon-boot-image
[glossary.component]: /docs/glossary#README.md#component
[glossary.driver manager]: /docs/glossary#README.md#driver-manager
[glossary.driver host]: /docs/glossary#README.md#driver-host
[glossary.fvm]: /docs/glossary#README.md#fuchsia-volume-manager
[glossary.realm]: /docs/glossary#README.md#realm
[glossary.session]: /docs/glossary#README.md#session-component
[glossary.outgoing-directory]: /docs/glossary/README.md#outgoing-directory
[blobfs]: /docs/concepts/filesystems/blobfs.md
[bootloader-and-kernel]: /docs/concepts/process/userboot.md#boot_loader_and_kernel_startup
[component-manager]: /docs/concepts/components/v2/introduction.md#component-manager
[critical-processes]: /reference/syscalls/job_set_critical.md
[devfs]: /docs/development/drivers/concepts/device_driver_model/device-model.md
[driver-communication]: /docs/concepts/drivers/driver_communication.md
[driver-manager-exposes]: /src/devices/bin/driver_manager/meta/driver-manager-base.shard.cml?l=83&drc=2deafe53a4a1626b5be2263c13e3dd57024be7db
[dynamic-linking]: https://en.wikipedia.org/wiki/Dynamic_linker
[fs-mount]: /docs/concepts/filesystems/filesystems.md#mounting
[fshost-exposes]: /src/storage/fshost/meta/base_fshost.cml?l=73&drc=fe4110848aaad498a65278ac05d65fd8201f5ca2
[fshost-magic-headers]: /src/storage/lib/fs_management/cpp/format.cc?l=60-61&drc=de246ad54cb9d6aab36ab29b48fdae69820c814e
[fuchsia-io]: https://fuchsia.dev/reference/fidl/fuchsia.io
[job]: /docs/reference/kernel_objects/job.md
[kernel-command-line]: /docs/reference/kernel/kernel_cmdline.md
[memfs]: /docs/concepts/filesystems/filesystems.md#memfs_an_in-memory_filesystem
[micro-kernel]: https://en.wikipedia.org/wiki/Microkernel
[minfs]: /docs/concepts/filesystems/minfs.md
[process-bootstrap]: /docs/concepts/process/program_loading.md
[userboot-loading]: /docs/concepts/process/userboot.md#kernel_loads_userboot
[userboot]: /docs/concepts/process/userboot.md
[userspace]: https://en.wikipedia.org/wiki/User_space
[wait-for-system]: /src/devices/bin/driver_manager/driver_loader.cc?l=123&drc=62174108e02c85feb7a18df5cc03dcf8ec7d8625
[zxcrypt]: /docs/concepts/filesystems/zxcrypt.md
