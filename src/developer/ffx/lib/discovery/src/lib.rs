// Copyright 2021 The Fuchsia Authors. All rights 1eserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::emulator_watcher::EmulatorWatcher;
use crate::error::Result;
pub use crate::events::{
    FastbootConnectionState, FastbootTargetState, TargetEvent, TargetHandle, TargetState,
};
use crate::fastboot_file_watcher::FastbootWatcher;
use crate::query::TargetInfoQuery;
use bitflags::bitflags;
use futures::Stream;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use manual_targets::watcher::{
    ManualTargetEvent, ManualTargetEventHandler, ManualTargetWatcher,
    recommended_watcher as manual_recommended_watcher,
};
use mdns_discovery::{MdnsEventHandler, MdnsWatcher, recommended_watcher};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use usb_fastboot_discovery::{
    FastbootEvent, FastbootEventHandler, FastbootUsbWatcher,
    recommended_watcher as fastboot_watcher,
};
// TODO(colnnelson): Long term it would be nice to have this be pulled into the mDNS library
// so that it can speak our language. Or even have the mdns library not export FIDL structs
// but rather some other well-defined type
use fidl_fuchsia_developer_ffx as ffx;

pub mod desc;
mod emulator_watcher;
pub mod error;
pub mod events;
mod fastboot_file_watcher;
pub mod query;

#[allow(dead_code)]
/// A stream of new devices as they appear on the bus. See [`wait_for_devices`].
pub struct TargetStream {
    /// The query for filtering which target to select.
    query: TargetInfoQuery,

    /// Watches mdns events
    mdns_watcher: Option<MdnsWatcher>,

    /// Watches for FastbootUsb events
    fastboot_usb_watcher: Option<FastbootUsbWatcher>,

    /// Watches for ManualTarget events
    manual_targets_watcher: Option<ManualTargetWatcher>,

    /// Watches for Emulator events
    emulator_watcher: Option<EmulatorWatcher>,

    /// Watches for Emulator events
    fastboot_file_watcher: Option<FastbootWatcher>,

    /// This is where results from the various watchers are published.
    queue: UnboundedReceiver<TargetEvent>,

    /// Whether we want to get Added events.
    notify_added: bool,

    /// Whether we want to get Removed events.
    notify_removed: bool,
}

pub struct TargetStreamConfig<Mdns, Fusb, Man>
where
    Mdns: MdnsEventHandler,
    Fusb: FastbootEventHandler,
    Man: ManualTargetEventHandler,
{
    /// The filter for the target stream. If constructing the stream and this is set to `None`,
    /// this will default to `TargetInfoQuery::First`.
    pub query: Option<TargetInfoQuery>,

    /// MDNS event handler.
    pub mdns_event_handler: Option<Mdns>,

    /// Fastboot USB event handler.
    pub fastboot_event_handler: Option<Fusb>,

    /// Manual target watcher.
    pub manual_targets_event_handler: Option<Man>,

    /// Emulator watcher.
    pub emulator_watcher: Option<EmulatorWatcher>,

    /// Fastboot file watcher.
    pub fastboot_file_watcher: Option<FastbootWatcher>,

    /// Should we notify when adding an item.
    pub notify_added: bool,

    /// Should we notify when removing an item.
    pub notify_removed: bool,
}

impl<Mdns, Fusb, Man> TargetStreamConfig<Mdns, Fusb, Man>
where
    Mdns: MdnsEventHandler,
    Fusb: FastbootEventHandler,
    Man: ManualTargetEventHandler,
{
    pub fn new() -> Self {
        // The type constraints make doing a derive of Default not doable.
        Self {
            query: None,
            mdns_event_handler: None,
            fastboot_event_handler: None,
            manual_targets_event_handler: None,
            emulator_watcher: None,
            fastboot_file_watcher: None,
            notify_added: false,
            notify_removed: false,
        }
    }

    pub fn set_query(&mut self, q: TargetInfoQuery) {
        self.query = Some(q)
    }

    pub fn set_mdns_event_handler(&mut self, e: Mdns) {
        self.mdns_event_handler = Some(e);
    }

    pub fn set_fastboot_event_handler(&mut self, e: Fusb) {
        self.fastboot_event_handler = Some(e);
    }

    pub fn set_manual_event_handler(&mut self, e: Man) {
        self.manual_targets_event_handler = Some(e);
    }

    pub fn set_emulator_watcher(&mut self, e: EmulatorWatcher) {
        self.emulator_watcher = Some(e);
    }

    pub fn set_fastboot_file_watcher(&mut self, f: FastbootWatcher) {
        self.fastboot_file_watcher = Some(f)
    }

    pub fn set_notify_removed(&mut self, n: bool) {
        self.notify_removed = n;
    }

    pub fn set_notify_added(&mut self, n: bool) {
        self.notify_added = n;
    }
}

impl TargetStream {
    /// Constructs a new target stream using the config, with each watcher defaulting to the
    /// recommended one.
    pub fn new<M, F, Man>(
        config: TargetStreamConfig<M, F, Man>,
        queue: UnboundedReceiver<TargetEvent>,
    ) -> Self
    where
        M: MdnsEventHandler,
        F: FastbootEventHandler,
        Man: ManualTargetEventHandler,
    {
        Self {
            query: config.query.unwrap_or(TargetInfoQuery::First),
            notify_added: config.notify_added,
            notify_removed: config.notify_removed,
            mdns_watcher: config.mdns_event_handler.map(|e| recommended_watcher(e)),
            fastboot_usb_watcher: config.fastboot_event_handler.map(|e| fastboot_watcher(e)),
            manual_targets_watcher: config
                .manual_targets_event_handler
                .map(|e| manual_recommended_watcher(e)),
            emulator_watcher: config.emulator_watcher,
            fastboot_file_watcher: config.fastboot_file_watcher,
            queue,
        }
    }
}

pub trait TargetEventStream: Stream<Item = TargetEvent> + std::marker::Unpin {}

impl TargetEventStream for TargetStream {}

pub trait TargetDiscovery {
    fn discover_devices(&self, query: TargetInfoQuery) -> Result<impl TargetEventStream>;
}

pub struct DiscoveryBuilder {
    emulator_instance_root: Option<PathBuf>,
    fastboot_devices_file_path: Option<PathBuf>,
    notify_added: bool,
    notify_removed: bool,
    sources: DiscoverySources,
}

impl DiscoveryBuilder {
    pub fn set_source(mut self, source: DiscoverySources) -> Self {
        self.sources = source;
        self
    }

    #[cfg(test)]
    pub fn with_source(mut self, source: DiscoverySources) -> Self {
        self.sources.insert(source);
        self
    }

    pub fn with_emulator_instance_root(mut self, emulator_instance_root: Option<PathBuf>) -> Self {
        if emulator_instance_root.is_some() {
            self.emulator_instance_root = emulator_instance_root;
            self.sources.insert(DiscoverySources::EMULATOR);
        }
        self
    }

    pub fn with_fastboot_devices_file_path(
        mut self,
        fastboot_devices_file_path: Option<PathBuf>,
    ) -> Self {
        if fastboot_devices_file_path.is_some() {
            self.fastboot_devices_file_path = fastboot_devices_file_path;
            self.sources.insert(DiscoverySources::FASTBOOT_FILE);
        }
        self
    }

    pub fn notify_added(mut self, notify_added: bool) -> Self {
        self.notify_added = notify_added;
        self
    }

    pub fn notify_removed(mut self, notify_removed: bool) -> Self {
        self.notify_removed = notify_removed;
        self
    }

    pub fn build(self) -> Discovery {
        Discovery {
            emulator_instance_root: self.emulator_instance_root,
            fastboot_devices_file_path: self.fastboot_devices_file_path,
            notify_added: self.notify_added,
            notify_removed: self.notify_removed,
            sources: self.sources,
        }
    }
}

impl Default for DiscoveryBuilder {
    fn default() -> Self {
        Self {
            emulator_instance_root: None,
            fastboot_devices_file_path: None,
            notify_added: true,
            notify_removed: true,
            sources: DiscoverySources::default(),
        }
    }
}

pub struct Discovery {
    emulator_instance_root: Option<PathBuf>,
    fastboot_devices_file_path: Option<PathBuf>,
    notify_added: bool,
    notify_removed: bool,
    sources: DiscoverySources,
}

impl Discovery {
    pub fn builder() -> DiscoveryBuilder {
        DiscoveryBuilder::default()
    }
}

impl TargetDiscovery for Discovery {
    #[allow(refining_impl_trait)]
    fn discover_devices(&self, query: TargetInfoQuery) -> Result<TargetStream> {
        let stream = wait_for_devices(
            query,
            self.emulator_instance_root.clone(),
            self.fastboot_devices_file_path.clone(),
            self.notify_added,
            self.notify_removed,
            self.sources,
        )?;
        Ok(stream)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DiscoverySources: u8 {
        const MDNS = 1 << 0;
        const USB_FASTBOOT = 1 << 1;
        const MANUAL = 1 << 2;
        const EMULATOR = 1 << 3;
        const FASTBOOT_FILE = 1 << 4;
    }
}

impl Default for DiscoverySources {
    fn default() -> Self {
        DiscoverySources::all()
    }
}

fn wait_for_devices(
    query: TargetInfoQuery,
    emulator_instance_root: Option<PathBuf>,
    fastboot_devices_file_path: Option<PathBuf>,
    notify_added: bool,
    notify_removed: bool,
    sources: DiscoverySources,
) -> Result<TargetStream> {
    let mut config = TargetStreamConfig::new();
    config.set_query(query);
    config.set_notify_added(notify_added);
    config.set_notify_removed(notify_removed);
    let (sender, queue) = unbounded();
    if sources.contains(DiscoverySources::MDNS) {
        let mdns_sender = sender.clone();
        config.set_mdns_event_handler(move |res: ffx::MdnsEventType| {
            // Translate the result to a TargetEvent
            let event = TargetEvent::try_from(res);
            if let Ok(event) = event {
                let _ = mdns_sender.unbounded_send(event);
            }
        })
    }

    // USB Fastboot watcher
    if sources.contains(DiscoverySources::USB_FASTBOOT) {
        let fastboot_sender = sender.clone();
        config.set_fastboot_event_handler(move |res: FastbootEvent| {
            // Translate the result to a TargetEvent
            log::debug!("discovery watcher got fastboot event: {:#?}", res);
            let event = res.into();
            let _ = fastboot_sender.unbounded_send(event);
        })
    }

    if sources.contains(DiscoverySources::MANUAL) {
        let manual_targets_sender = sender.clone();
        config.set_manual_event_handler(move |res: ManualTargetEvent| {
            // Translate the result to a TargetEvent
            log::trace!("discovery watcher got manual target event: {:#?}", res);
            let event = res.into();
            let _ = manual_targets_sender.unbounded_send(event);
        })
    }

    if sources.contains(DiscoverySources::EMULATOR) {
        if let Some(instance_root) = emulator_instance_root {
            config.set_emulator_watcher(EmulatorWatcher::new(instance_root, sender.clone())?)
        }
    }

    if sources.contains(DiscoverySources::FASTBOOT_FILE) {
        if let Some(fastboot_devices_file) = fastboot_devices_file_path {
            config.set_fastboot_file_watcher(FastbootWatcher::new(fastboot_devices_file, sender)?)
        }
    }

    Ok(TargetStream::new(config, queue))
}

impl Stream for TargetStream {
    type Item = TargetEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(event) = ready!(Pin::new(&mut self.queue).poll_next(cx)) else {
            return Poll::Ready(None);
        };

        let should_notify_matches = self.query.match_handle(event.as_handle());
        let should_notify_added = event.is_added() && self.notify_added;
        let should_notify_removed = event.is_removed() && self.notify_removed;
        let should_notify = should_notify_matches && (should_notify_added || should_notify_removed);
        if should_notify {
            return Poll::Ready(Some(event));
        }
        // Important: must schedule the future for this to be woken up again.
        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use addr::TargetAddr;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs::File;
    use std::io::Write;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::rc::Rc;
    use std::str::FromStr;

    /// Used for testing functions that take a TargetDiscovery
    ///
    /// In discover_devices the `query` parameter is explicitly not used.
    /// Test authors are expected to pass the VecDeque with the events "pre filtered"
    /// You should have separate tests for your queries
    pub struct TestDiscovery {
        events: Rc<RefCell<VecDeque<TargetEvent>>>,
    }

    impl TargetDiscovery for TestDiscovery {
        #[allow(refining_impl_trait)]

        fn discover_devices(
            &self,
            _query: TargetInfoQuery,
        ) -> crate::error::Result<TestTargetStream> {
            Ok(TestTargetStream { events: self.events.clone() })
        }
    }

    pub struct TestTargetStream {
        events: Rc<RefCell<VecDeque<TargetEvent>>>,
    }

    impl TargetEventStream for TestTargetStream {}

    impl Stream for TestTargetStream {
        type Item = TargetEvent;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            let event = self.events.borrow_mut().pop_front();
            Poll::Ready(event)
        }
    }

    ///////////////////////////////////////////////////////////////////////////
    // Example TestDiscovery Usage
    ///////////////////////////////////////////////////////////////////////////

    fn write_target_event<W: Write>(writer: &mut W, event: TargetEvent) -> std::io::Result<()> {
        let symbol = match event {
            TargetEvent::Added(_) => "+",
            TargetEvent::Removed(_) => "-",
        };

        let handle = event.as_handle();
        let node_name = handle.node_name.as_ref().map_or(target_errors::UNKNOWN_TARGET_NAME, |v| v);
        let state = &handle.state;

        writeln!(writer, "{symbol}  {node_name}  {state}")?;
        Ok(())
    }

    /// Writes the events in a target event stream to the given writer
    async fn write_event_stream(writer: &mut impl Write, mut stream: impl TargetEventStream) {
        while let Some(event) = stream.next().await {
            let _ = write_target_event(writer, event);
        }
    }

    #[fuchsia::test]
    async fn test_write_event_stream() -> anyhow::Result<()> {
        let mut writer = vec![];
        let disco = TestDiscovery {
            events: Rc::new(RefCell::new(VecDeque::from([
                TargetEvent::Added(TargetHandle {
                    node_name: Some("magnus".to_string()),
                    state: TargetState::Unknown,
                    manual: false,
                }),
                TargetEvent::Added(TargetHandle {
                    node_name: Some("abagail".to_string()),
                    state: TargetState::Unknown,
                    manual: false,
                }),
                TargetEvent::Removed(TargetHandle {
                    node_name: Some("abagail".to_string()),
                    state: TargetState::Unknown,
                    manual: false,
                }),
            ]))),
        };

        let stream = disco.discover_devices(TargetInfoQuery::First)?;

        write_event_stream(&mut writer, stream).await;

        assert_eq!(
            String::from_utf8(writer).expect("we write uft8"),
            r#"+  magnus  Unknown
+  abagail  Unknown
-  abagail  Unknown
"#
        );

        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    // Test DiscoveryBuilder
    ///////////////////////////////////////////////////////////////////////////

    #[test]
    fn test_discovery_builder_default() -> anyhow::Result<()> {
        let discovery = Discovery::builder().build();
        assert_eq!(discovery.notify_added, true);
        assert_eq!(discovery.notify_removed, true);
        assert_eq!(discovery.sources, DiscoverySources::all());
        assert!(discovery.emulator_instance_root.is_none());
        Ok(())
    }

    #[test]
    fn test_discovery_builder_changes() -> anyhow::Result<()> {
        let discovery = Discovery::builder()
            .notify_added(false)
            .notify_removed(false)
            .set_source(DiscoverySources::MANUAL)
            .with_source(DiscoverySources::EMULATOR)
            .set_source(DiscoverySources::MDNS)
            .with_source(DiscoverySources::USB_FASTBOOT)
            .build();
        assert_eq!(discovery.notify_added, false);
        assert_eq!(discovery.notify_removed, false);
        assert_eq!(discovery.sources, DiscoverySources::USB_FASTBOOT | DiscoverySources::MDNS);
        assert!(discovery.emulator_instance_root.is_none());
        Ok(())
    }

    #[test]
    fn test_discovery_builder_with_root() -> anyhow::Result<()> {
        let discovery = Discovery::builder()
            .set_source(DiscoverySources::MANUAL)
            .with_emulator_instance_root(Some(
                PathBuf::from_str("/tmp").expect("tmp is a valid path"),
            ))
            .build();

        assert_eq!(discovery.notify_added, true);
        assert_eq!(discovery.notify_removed, true);
        assert_eq!(discovery.sources, DiscoverySources::MANUAL | DiscoverySources::EMULATOR);
        assert_eq!(
            discovery.emulator_instance_root,
            Some(PathBuf::from_str("/tmp").expect("tmp is a valid path"))
        );
        Ok(())
    }

    ///////////////////////////////////////////////////////////////////////////
    ///  TargetStream tests
    ///////////////////////////////////////////////////////////////////////////

    #[fuchsia::test]
    async fn test_target_stream() -> anyhow::Result<()> {
        let (sender, queue) = unbounded();

        let mut stream = TargetStream {
            query: TargetInfoQuery::First,
            mdns_watcher: None,
            fastboot_usb_watcher: None,
            manual_targets_watcher: None,
            emulator_watcher: None,
            fastboot_file_watcher: None,
            queue,
            notify_added: true,
            notify_removed: true,
        };

        // Send a few events
        sender
            .unbounded_send(TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        sender
            .unbounded_send(TargetEvent::Removed(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        assert_eq!(
            stream.next().await.unwrap(),
            TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            })
        );

        assert_eq!(
            stream.next().await.unwrap(),
            TargetEvent::Removed(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            })
        );

        Ok(())
    }

    #[fuchsia::test]
    async fn test_target_stream_ignores_added() -> anyhow::Result<()> {
        let (sender, queue) = unbounded();

        let mut stream = TargetStream {
            query: TargetInfoQuery::First,
            mdns_watcher: None,
            fastboot_usb_watcher: None,
            manual_targets_watcher: None,
            emulator_watcher: None,
            fastboot_file_watcher: None,
            queue,
            notify_added: false,
            notify_removed: true,
        };

        // Send a few events
        sender
            .unbounded_send(TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        sender
            .unbounded_send(TargetEvent::Removed(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        assert_eq!(
            stream.next().await.unwrap(),
            TargetEvent::Removed(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            })
        );

        Ok(())
    }

    #[fuchsia::test]
    async fn test_target_stream_ignores_removed() -> anyhow::Result<()> {
        let (sender, queue) = unbounded();

        let mut stream = TargetStream {
            query: TargetInfoQuery::First,
            mdns_watcher: None,
            fastboot_usb_watcher: None,
            manual_targets_watcher: None,
            emulator_watcher: None,
            fastboot_file_watcher: None,
            queue,
            notify_added: true,
            notify_removed: false,
        };

        // Send a few events
        sender
            .unbounded_send(TargetEvent::Removed(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        sender
            .unbounded_send(TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        assert_eq!(
            stream.next().await.unwrap(),
            TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            })
        );

        Ok(())
    }

    #[fuchsia::test]
    async fn test_target_stream_filtered() -> anyhow::Result<()> {
        let (sender, queue) = unbounded();

        let mut stream = TargetStream {
            query: TargetInfoQuery::NodenameOrSerial("Vin".to_string()),
            mdns_watcher: None,
            fastboot_usb_watcher: None,
            manual_targets_watcher: None,
            emulator_watcher: None,
            fastboot_file_watcher: None,
            queue,
            notify_added: true,
            notify_removed: true,
        };

        // Send a few events
        let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let addr = TargetAddr::from(socket);
        // This should not come into the queue since the target is not in zedboot
        sender
            .unbounded_send(TargetEvent::Added(TargetHandle {
                node_name: Some("Kelsier".to_string()),
                state: TargetState::Product { addrs: vec![addr], serial: None },
                manual: false,
            }))
            .unwrap();

        sender
            .unbounded_send(TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            }))
            .unwrap();

        assert_eq!(
            stream.next().await.unwrap(),
            TargetEvent::Added(TargetHandle {
                node_name: Some("Vin".to_string()),
                state: TargetState::Zedboot,
                manual: false,
            })
        );

        Ok(())
    }

    fn build_instance_file(dir: &PathBuf, name: &str) -> std::io::Result<File> {
        let new_instance_dir = dir.join(String::from(name));
        std::fs::create_dir_all(&new_instance_dir)?;
        let new_instance_engine_file = new_instance_dir.join("engine.json");
        use emulator_instance::EmulatorInstanceInfo;
        // Build the expected config JSON contents
        let mut instance_data = emulator_instance::EmulatorInstanceData::new_with_state(
            name,
            emulator_instance::EngineState::Running,
        );
        instance_data.set_pid(std::process::id());
        let config = instance_data.get_emulator_configuration_mut();
        config.host.networking = emulator_instance::NetworkingMode::User;
        config.host.port_map.insert(
            String::from("ssh"),
            emulator_instance::PortMapping { guest: 22, host: Some(3322) },
        );
        let config_str = serde_json::to_string(&instance_data)?;
        let mut config_file = File::create(&new_instance_engine_file)?;
        config_file.write_all(config_str.as_bytes())?;
        config_file.flush()?;
        Ok(config_file)
    }

    // This test has a race condition, which I (slgrady) have spent hours trying to find.
    // Apparently the notify crate in emulator_instance is for some reason not producing
    // the Create event for the new emulator file. The event is created in a separate
    // thread, so it's not an async issue. The watcher is not being dropped at that point.
    // The file is in fact created and placed on the filesystem; the watcher thread
    // is running at the point we are waiting for the event.  Giving up, since I believe
    // the race-condition only comes up in the artificial environment of a test case.
    // Normally, emulators are extended events, not just a fast creation of a single file.
    #[ignore]
    #[fuchsia::test]
    async fn test_target_stream_produces_emulator() -> anyhow::Result<()> {
        use tempfile::tempdir;

        let _env = ffx_config::test_init().await.expect("Failed to initialize test env");

        // Create the emulator instance dir
        let temp = tempdir().expect("cannot get tempdir");
        let instance_dir = temp.path().to_path_buf();
        let emu_instances = emulator_instance::EmulatorInstances::new(instance_dir.clone());

        // Add a new emulator
        let config_file = build_instance_file(&instance_dir, "emu-data-instance")?;

        // Before waiting on devices, let's make sure we're actually getting the
        // emulator. (This shouldn't be necessary, but I've seen this test flake
        // by timing out, so this is a validity check.)
        let existing = emulator_instance::get_all_targets(&emu_instances).unwrap();
        assert_eq!(existing.len(), 1);

        // Start watching the directory
        let mut stream = wait_for_devices(
            TargetInfoQuery::First,
            Some(instance_dir.clone()),
            None,
            true,
            false,
            DiscoverySources::EMULATOR,
        )
        .unwrap();

        // Assert that the existing emulator is discovered
        let next =
            stream.next().await.expect("No event was waiting after watching for existing emulator");
        assert_eq!(
            next,
            // The node_name and the state both have to match the contents of the emu_config above.
            TargetEvent::Added(TargetHandle {
                // Name must correspond to "runtime:name" value in config
                node_name: Some("emu-data-instance".to_string()),
                // Addr must correspond to "host:port_map:sh:host" value in config
                state: TargetState::Product {
                    addrs: vec![TargetAddr::from_str("127.0.0.1:3322").unwrap()],
                    serial: None
                },
                manual: false,
            })
        );

        // Add a new (different) emulator
        let config_file2 = build_instance_file(&instance_dir, "emu-data-instance2")?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        // let existing = emulator_instance::get_all_targets().await?;
        // assert_eq!(existing.len(), 2);

        // Assert that the newly-created emulator is discovered
        let next =
            stream.next().await.expect("No event was waiting after watching for new emulator");
        assert_eq!(
            next,
            // The node_name and the state both have to match the contents of the emu_config above.
            TargetEvent::Added(TargetHandle {
                // Name must correspond to "runtime:name" value in config
                node_name: Some("emu-data-instance2".to_string()),
                // Addr must correspond to "host:port_map:sh:host" value in config
                state: TargetState::Product {
                    addrs: vec![TargetAddr::from_str("127.0.0.1:3322").unwrap()],
                    serial: None
                },
                manual: false,
            })
        );

        drop(config_file);
        drop(config_file2);
        std::fs::remove_dir_all(&instance_dir)?;
        // TODO(325325761) -- re-enable when emulator Remove events are generated
        // correctly.
        // let next = stream
        //     .next()
        //     .await
        //     .unwrap();
        // let next = next.expect("Getting emulator event failed");
        // assert_eq!(
        //     next,
        //     // The node_name and the state both have to match the contents of the emu_config above.
        //     TargetEvent::Removed(TargetHandle {
        //         // Name must correspond to "runtime:name" value in config
        //         node_name: Some("fuchsia-emulator".to_string()),
        //         // Addr must correspond to "host:port_map:sh:host" value in config
        //         state: TargetState::Product(TargetAddr::from_str("127.0.0.1:33881")?),
        //     })
        // );

        Ok(())
    }
}
