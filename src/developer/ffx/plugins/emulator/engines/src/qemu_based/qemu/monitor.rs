// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A client for the QEMU Human Monitor Protocol (HMP), which QEMU serves on the monitor socket
//! in the emulator's instance directory. It is used to save, load and list snapshots of a
//! running emulator. The commands are documented in
//! https://www.qemu.org/docs/master/system/monitor.html.

use crate::qemu_based::comms::QemuSocket;
use ffx_emulator_config::SnapshotInfo;
use fho::{Result, bug, return_bug, return_user_error, user_error};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The prompt which the monitor prints when it is ready to read a command.
const PROMPT: &[u8] = b"(qemu) ";

/// The prefix of the lines which the monitor prints when a command fails.
const ERROR_PREFIX: &str = "Error: ";

/// The status which the monitor reports for an emulator that was paused to load a snapshot, and
/// was not resumed because the snapshot could not be loaded.
const PAUSED_FOR_RESTORE_STATUS: &str = "paused (restore-vm)";

/// How long to wait for the monitor to greet a new connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for commands which only query the state of the emulator.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for a snapshot to be saved or loaded, at the least.
const SNAPSHOT_BASE_TIMEOUT: Duration = Duration::from_secs(60);

/// How much longer to wait for a snapshot to be saved or loaded, for each GiB of the guest's
/// memory. This allows for a disk which writes, or reads, as little as 35 MB/s.
const SNAPSHOT_TIMEOUT_PER_GIB: Duration = Duration::from_secs(30);

/// Returns how long to wait for a snapshot of a guest which has `memory_bytes` of memory to be
/// saved or loaded. The emulator has to write, or read, all of the guest's memory which is in
/// use, so this takes longer for larger guests.
pub(crate) fn snapshot_timeout(memory_bytes: u64) -> Duration {
    let gib = u32::try_from(memory_bytes.div_ceil(1 << 30)).unwrap_or(u32::MAX);
    SNAPSHOT_BASE_TIMEOUT.saturating_add(SNAPSHOT_TIMEOUT_PER_GIB.saturating_mul(gib))
}

/// A connection to the monitor of a running QEMU instance.
pub(crate) struct Monitor {
    stream: UnixStream,
    /// How long to wait for a snapshot to be saved or loaded.
    snapshot_timeout: Duration,
}

impl Monitor {
    /// Connects to the monitor socket at `socket_path`, and waits for the monitor to be ready to
    /// read a command. Snapshots which are saved or loaded through the connection are given
    /// `snapshot_timeout` to complete; see `snapshot_timeout()`.
    pub(crate) fn connect(socket_path: &PathBuf, snapshot_timeout: Duration) -> Result<Self> {
        let mut socket = QemuSocket::new(socket_path);
        socket
            .connect()
            .map_err(|e| user_error!("Could not connect to the emulator's monitor: {e}"))?;
        let stream = socket.stream().ok_or_else(|| bug!("No monitor socket connected."))?;
        let mut monitor = Self { stream, snapshot_timeout };

        // The monitor greets each new connection with a banner, followed by its prompt. It only
        // serves a single connection at a time, so nothing is received for as long as another
        // client is connected to it, nor while the emulator is busy saving or loading a snapshot.
        monitor.read_until_prompt(CONNECT_TIMEOUT)?.ok_or_else(|| {
            user_error!(
                "Timed out waiting for the emulator's monitor to respond. The monitor doesn't \
                respond while another client, such as `ffx emu console --command`, is attached \
                to it, or while the emulator is saving or loading a snapshot."
            )
        })?;
        Ok(monitor)
    }

    /// Saves a snapshot of the emulator under the given name, replacing any existing snapshot
    /// with that name.
    pub(crate) fn save_snapshot(&mut self, name: &str) -> Result<()> {
        let output = self.execute(&format!("savevm {name}"), self.snapshot_timeout)?;
        check_output(&output).map_err(|e| user_error!("Could not save snapshot '{name}': {e}"))
    }

    /// Restores the emulator to the state saved in the snapshot with the given name.
    pub(crate) fn load_snapshot(&mut self, name: &str) -> Result<()> {
        // QEMU pauses the emulator before it tries to load a snapshot, and leaves it paused if
        // that fails. Check that the snapshot exists first, so that a mistyped name doesn't
        // interrupt the emulator.
        let snapshots = self.list_snapshots()?;
        if !snapshots.iter().any(|snapshot| snapshot.name == name) {
            let names: Vec<&str> =
                snapshots.iter().map(|snapshot| snapshot.name.as_str()).collect();
            let available = if names.is_empty() {
                "No snapshots have been saved for this emulator.".to_string()
            } else {
                format!("The available snapshots are: {}.", names.join(", "))
            };
            return_user_error!("Snapshot '{name}' does not exist. {available}");
        }

        let output = self.execute(&format!("loadvm {name}"), self.snapshot_timeout)?;
        check_output(&output).map_err(|e| {
            user_error!(
                "Could not load snapshot '{name}': {e}\n\
                The emulator has been left paused, because its state may be inconsistent after \
                the failed load. Load a different snapshot, or stop the emulator with \
                `ffx emu stop`."
            )
        })?;

        // The emulator carries on running by itself once the snapshot is loaded, unless it had
        // been left paused by an earlier load which failed. In that case it has to be resumed.
        let status = self.execute("info status", QUERY_TIMEOUT)?;
        if status.contains(PAUSED_FOR_RESTORE_STATUS) {
            let output = self.execute("cont", QUERY_TIMEOUT)?;
            check_output(&output).map_err(|e| {
                user_error!("Loaded snapshot '{name}', but could not resume the emulator: {e}")
            })?;
        }
        Ok(())
    }

    /// Returns the snapshots which the emulator can be restored to.
    pub(crate) fn list_snapshots(&mut self) -> Result<Vec<SnapshotInfo>> {
        let output = self.execute("info snapshots", QUERY_TIMEOUT)?;
        check_output(&output).map_err(|e| user_error!("Could not list snapshots: {e}"))?;
        parse_snapshot_list(&output)
    }

    /// Runs a single monitor command, and returns its output.
    fn execute(&mut self, command: &str, timeout: Duration) -> Result<String> {
        log::debug!("Sending monitor command: {command}");
        self.stream
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|e| bug!("Error writing to the emulator's monitor: {e}"))?;
        let response = self.read_until_prompt(timeout)?.ok_or_else(|| {
            user_error!(
                "Timed out after {} seconds waiting for the emulator to complete the monitor \
                command '{command}'. The emulator may still be carrying it out, and its monitor \
                doesn't respond to other commands until it is done.",
                timeout.as_secs()
            )
        })?;
        let output = parse_response(&response);
        log::debug!("Monitor command output: {output}");
        Ok(output)
    }

    /// Reads from the monitor until it prints its prompt, which indicates that it is ready to
    /// read the next command. Returns None if the prompt is not received within `timeout`.
    fn read_until_prompt(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        let mut response = vec![];
        let mut buffer = [0; 4096];
        while !response.ends_with(PROMPT) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| bug!("Error setting the timeout of the monitor socket: {e}"))?;
            match self.stream.read(&mut buffer) {
                Ok(0) => return_bug!("The emulator's monitor closed the connection."),
                Ok(length) => response.extend_from_slice(&buffer[..length]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => (),
                Err(e) => return_bug!("Error reading from the emulator's monitor: {e}"),
            }
        }
        Ok(Some(response))
    }
}

/// Extracts the output of a command from the monitor's raw response to it.
///
/// The monitor is meant for interactive use: it echoes the characters of the command back as it
/// reads them, interleaved with terminal control sequences which redraw the line. The command's
/// own output starts on the line after that echo, and is followed by the prompt.
fn parse_response(response: &[u8]) -> String {
    let response = response.strip_suffix(PROMPT).unwrap_or(response);
    let response = String::from_utf8_lossy(response);
    match response.split_once('\n') {
        Some((_echo, output)) => output.replace("\r\n", "\n").trim().to_string(),
        None => String::new(),
    }
}

/// Checks whether the output of a monitor command reports that the command failed, and if so
/// returns the reason for the failure.
///
/// Only lines which start with "Error: " count as a failure. That is how the HMP of the QEMU
/// which is bundled with Fuchsia (QEMU 11) reports the failure of the commands used here; older
/// versions, and other code paths, print failures as plain text which would be taken for
/// success. Check this again when QEMU is rolled.
fn check_output(output: &str) -> std::result::Result<(), String> {
    match output.lines().find_map(|line| line.strip_prefix(ERROR_PREFIX)) {
        Some(error) => Err(error.to_string()),
        None => Ok(()),
    }
}

/// Parses the output of the `info snapshots` monitor command, which looks like this:
///
/// ```text
/// List of snapshots present on all disks:
/// ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT
/// --      booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --
/// ```
///
/// When there are no snapshots, the output is `There is no snapshot available.` instead. If the
/// emulator has several disks, the output can also contain lists of the snapshots which are
/// only present on some of them; those are left out because they cannot be loaded.
fn parse_snapshot_list(output: &str) -> Result<Vec<SnapshotInfo>> {
    parse_snapshot_table(output, "List of snapshots present on all disks:")
}

/// Parses the table of snapshots which follows the line `heading` in `output`. QEMU prints the
/// same table wherever it lists snapshots, which includes the `info snapshots` monitor command and
/// `qemu-img snapshot -l`; the monitor shows "--" instead of the IDs of the snapshots:
///
/// ```text
/// ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT
/// 1       booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --
/// ```
///
/// The table ends at the first empty line, or at the heading of another table. If `heading` is
/// not found, there are no snapshots.
pub(super) fn parse_snapshot_table(output: &str, heading: &str) -> Result<Vec<SnapshotInfo>> {
    const COLUMNS_HEADING: &str = "ID";

    let mut snapshots = vec![];
    let rows = output
        .lines()
        .skip_while(|line| line.trim() != heading)
        .skip(1)
        .take_while(|line| !line.trim().is_empty() && !line.trim_end().ends_with(':'));
    for line in rows {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Skip the heading of the table, and the placeholder which the monitor prints instead of
        // the table when only some of the disks have snapshots.
        if fields.first() == Some(&COLUMNS_HEADING) || fields == ["None"] {
            continue;
        }
        // The name is the only field which can contain spaces, so the fields which follow it are
        // located by counting back from the end of the line: the two parts of the size, the
        // date, the time, the clock and the instruction count.
        let [_id, name @ .., size, size_units, date, time, vm_clock, _icount] = &fields[..] else {
            return_bug!("Unexpected snapshot description from QEMU: '{line}'");
        };
        if name.is_empty() {
            return_bug!("Unexpected snapshot description from QEMU: '{line}'");
        }
        snapshots.push(SnapshotInfo {
            name: name.join(" "),
            vm_size: format!("{size} {size_units}"),
            date: format!("{date} {time}"),
            vm_clock: vm_clock.to_string(),
        });
    }
    Ok(snapshots)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;
    use tempfile::{TempDir, tempdir};

    const BANNER: &str = "QEMU 11.0.2 monitor - type 'help' for more information\r\n";
    const PROMPT_STR: &str = "(qemu) ";

    /// How long the fake monitor is given to save or load a snapshot.
    const TEST_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

    pub(crate) const SNAPSHOT_LIST: &str = "List of snapshots present on all disks:\r\n\
        ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\r\n\
        --      booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --\r\n\
        --      second           1.45 GiB 2026-10-01 08:15:00  0001:02:03.004         --\r\n";

    /// Reproduces the way the monitor echoes a command: each character which is typed causes the
    /// whole line to be redrawn using terminal control sequences.
    fn echo(command: &str) -> String {
        let mut echo = String::new();
        for length in 1..=command.len() {
            echo.push_str(&command[..length]);
            echo.push_str("\x1b[K");
            if length < command.len() {
                echo.push_str(&"\x1b[D".repeat(length));
            }
        }
        echo.push_str("\r\n");
        echo
    }

    /// Starts a fake monitor which listens on a socket in a temporary directory. It serves a
    /// single connection, replying to each of the expected commands with the given output, and
    /// returns the commands it received when the connection is closed.
    pub(crate) fn start_fake_monitor(
        replies: Vec<(&'static str, &'static str)>,
    ) -> (TempDir, PathBuf, JoinHandle<Vec<String>>) {
        let temp = tempdir().expect("tempdir");
        let socket_path = temp.path().join("monitor");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            stream.write_all(format!("{BANNER}{PROMPT_STR}").as_bytes()).expect("write banner");
            let mut received = vec![];
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut replies = replies.into_iter();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("read command") == 0 {
                    break;
                }
                let command = line.trim_end().to_string();
                let (expected, output) = replies.next().expect("unexpected command");
                assert_eq!(command, expected);
                stream
                    .write_all(format!("{}{output}{PROMPT_STR}", echo(&command)).as_bytes())
                    .expect("write reply");
                received.push(command);
            }
            received
        });
        (temp, socket_path, handle)
    }

    #[fuchsia::test]
    fn test_save_snapshot() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![("savevm booted", "")]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        monitor.save_snapshot("booted").expect("save snapshot");
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["savevm booted".to_string()]);
    }

    #[fuchsia::test]
    fn test_save_snapshot_error() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![(
            "savevm booted",
            "Error: State blocked by non-migratable CPU device (invtsc flag)\r\n",
        )]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let err = monitor.save_snapshot("booted").expect_err("save fails");
        assert_eq!(
            err.to_string(),
            "Could not save snapshot 'booted': State blocked by non-migratable CPU device \
            (invtsc flag)"
        );
        drop(monitor);
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_save_snapshot_times_out() {
        let temp = tempdir().expect("tempdir");
        let socket_path = temp.path().join("monitor");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        // The monitor reads the command, but doesn't complete it in time, as when the emulator
        // takes longer to save the snapshot than it is given.
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            stream.write_all(format!("{BANNER}{PROMPT_STR}").as_bytes()).expect("write banner");
            let mut reader = BufReader::new(stream);
            let mut command = String::new();
            reader.read_line(&mut command).expect("read command");
            // Keep the connection open until the client closes it.
            reader.read_to_string(&mut String::new()).expect("wait for the client to disconnect");
            command
        });
        let mut monitor =
            Monitor::connect(&socket_path, Duration::from_millis(50)).expect("connect to monitor");
        let err = monitor.save_snapshot("booted").expect_err("save times out").to_string();
        assert!(err.contains("to complete the monitor command 'savevm booted'"), "{err}");
        assert!(err.contains("The emulator may still be carrying it out"), "{err}");
        drop(monitor);
        assert_eq!(handle.join().unwrap(), "savevm booted\n");
    }

    #[fuchsia::test]
    fn test_load_snapshot() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![
            ("info snapshots", SNAPSHOT_LIST),
            ("loadvm booted", ""),
            ("info status", "VM status: running\r\n"),
        ]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        monitor.load_snapshot("booted").expect("load snapshot");
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["info snapshots", "loadvm booted", "info status"]);
    }

    #[fuchsia::test]
    fn test_load_snapshot_resumes_emulator_paused_by_failed_load() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![
            ("info snapshots", SNAPSHOT_LIST),
            ("loadvm booted", ""),
            ("info status", "VM status: paused (restore-vm)\r\n"),
            ("cont", ""),
        ]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        monitor.load_snapshot("booted").expect("load snapshot");
        drop(monitor);
        assert_eq!(
            handle.join().unwrap(),
            vec!["info snapshots", "loadvm booted", "info status", "cont"]
        );
    }

    #[fuchsia::test]
    fn test_load_snapshot_leaves_emulator_paused_by_user() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![
            ("info snapshots", SNAPSHOT_LIST),
            ("loadvm booted", ""),
            ("info status", "VM status: paused\r\n"),
        ]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        monitor.load_snapshot("booted").expect("load snapshot");
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["info snapshots", "loadvm booted", "info status"]);
    }

    #[fuchsia::test]
    fn test_load_snapshot_missing() {
        // The emulator is not asked to load a snapshot which doesn't exist.
        let (_temp, socket_path, handle) =
            start_fake_monitor(vec![("info snapshots", SNAPSHOT_LIST)]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let err = monitor.load_snapshot("missing").expect_err("load fails");
        assert_eq!(
            err.to_string(),
            "Snapshot 'missing' does not exist. The available snapshots are: booted, second."
        );
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["info snapshots"]);
    }

    #[fuchsia::test]
    fn test_load_snapshot_none_saved() {
        let (_temp, socket_path, handle) =
            start_fake_monitor(vec![("info snapshots", "There is no snapshot available.\r\n")]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let err = monitor.load_snapshot("booted").expect_err("load fails");
        assert_eq!(
            err.to_string(),
            "Snapshot 'booted' does not exist. No snapshots have been saved for this emulator."
        );
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["info snapshots"]);
    }

    #[fuchsia::test]
    fn test_load_snapshot_error() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![
            ("info snapshots", SNAPSHOT_LIST),
            ("loadvm booted", "Error: Missing section footer for 0000:00:1f.2/ich9_ahci\r\n"),
        ]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let err = monitor.load_snapshot("booted").expect_err("load fails").to_string();
        assert!(
            err.starts_with(
                "Could not load snapshot 'booted': Missing section footer for \
                0000:00:1f.2/ich9_ahci\n"
            ),
            "{err}"
        );
        assert!(err.contains("The emulator has been left paused"), "{err}");
        drop(monitor);
        assert_eq!(handle.join().unwrap(), vec!["info snapshots", "loadvm booted"]);
    }

    #[fuchsia::test]
    fn test_list_snapshots() {
        let (_temp, socket_path, handle) =
            start_fake_monitor(vec![("info snapshots", SNAPSHOT_LIST)]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let snapshots = monitor.list_snapshots().expect("list snapshots");
        assert_eq!(
            snapshots,
            vec![
                SnapshotInfo {
                    name: "booted".to_string(),
                    vm_size: "382 MiB".to_string(),
                    date: "2026-09-30 22:30:24".to_string(),
                    vm_clock: "0000:00:19.615".to_string(),
                },
                SnapshotInfo {
                    name: "second".to_string(),
                    vm_size: "1.45 GiB".to_string(),
                    date: "2026-10-01 08:15:00".to_string(),
                    vm_clock: "0001:02:03.004".to_string(),
                },
            ]
        );
        drop(monitor);
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_list_snapshots_error() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![(
            "info snapshots",
            "Error: no block device can store vmstate for snapshot\r\n",
        )]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        let err = monitor.list_snapshots().expect_err("list fails");
        assert_eq!(
            err.to_string(),
            "Could not list snapshots: no block device can store vmstate for snapshot"
        );
        drop(monitor);
        handle.join().unwrap();
    }

    #[fuchsia::test]
    fn test_multiple_commands_on_one_connection() {
        let (_temp, socket_path, handle) = start_fake_monitor(vec![
            ("savevm booted", ""),
            ("info snapshots", SNAPSHOT_LIST),
            ("savevm second", ""),
        ]);
        let mut monitor =
            Monitor::connect(&socket_path, TEST_SNAPSHOT_TIMEOUT).expect("connect to monitor");
        monitor.save_snapshot("booted").expect("save snapshot");
        assert_eq!(monitor.list_snapshots().expect("list snapshots").len(), 2);
        monitor.save_snapshot("second").expect("save snapshot");
        drop(monitor);
        assert_eq!(handle.join().unwrap().len(), 3);
    }

    #[fuchsia::test]
    fn test_connect_no_socket() {
        let temp = tempdir().expect("tempdir");
        let err = Monitor::connect(&temp.path().join("monitor"), TEST_SNAPSHOT_TIMEOUT)
            .err()
            .expect("connect fails");
        assert!(err.to_string().contains("Could not connect to the emulator's monitor"), "{err}");
    }

    #[fuchsia::test]
    fn test_read_until_prompt_times_out() {
        let temp = tempdir().expect("tempdir");
        let socket_path = temp.path().join("monitor");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let stream = UnixStream::connect(&socket_path).expect("connect");
        // Accept the connection, but never write the prompt to it.
        let (mut peer, _) = listener.accept().expect("accept connection");
        peer.write_all(BANNER.as_bytes()).expect("write banner");

        let mut monitor = Monitor { stream, snapshot_timeout: TEST_SNAPSHOT_TIMEOUT };
        let response =
            monitor.read_until_prompt(Duration::from_millis(50)).expect("no error on timeout");
        assert_eq!(response, None);
    }

    #[fuchsia::test]
    fn test_read_until_prompt_connection_closed() {
        let temp = tempdir().expect("tempdir");
        let socket_path = temp.path().join("monitor");
        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let stream = UnixStream::connect(&socket_path).expect("connect");
        let (peer, _) = listener.accept().expect("accept connection");
        drop(peer);

        let mut monitor = Monitor { stream, snapshot_timeout: TEST_SNAPSHOT_TIMEOUT };
        let err = monitor.read_until_prompt(Duration::from_secs(10)).expect_err("read fails");
        assert!(err.to_string().contains("closed the connection"), "{err}");
    }

    #[test]
    fn test_snapshot_timeout() {
        const GIB: u64 = 1 << 30;
        assert_eq!(snapshot_timeout(0), Duration::from_secs(60));
        assert_eq!(snapshot_timeout(GIB), Duration::from_secs(90));
        assert_eq!(snapshot_timeout(8 * GIB), Duration::from_secs(300));
        // Part of a GiB counts as a whole one.
        assert_eq!(snapshot_timeout(8 * GIB + 1), Duration::from_secs(330));
        // The timeout doesn't overflow for any amount of memory.
        assert!(snapshot_timeout(u64::MAX) > Duration::from_secs(1 << 32));
    }

    #[test]
    fn test_parse_response() {
        // A command without output.
        let response = format!("{}{PROMPT_STR}", echo("savevm booted"));
        assert_eq!(parse_response(response.as_bytes()), "");

        // A command with a single line of output.
        let response =
            format!("{}There is no snapshot available.\r\n{PROMPT_STR}", echo("info snapshots"));
        assert_eq!(parse_response(response.as_bytes()), "There is no snapshot available.");

        // A command with several lines of output.
        let response = format!("{}{SNAPSHOT_LIST}{PROMPT_STR}", echo("info snapshots"));
        assert_eq!(parse_response(response.as_bytes()), SNAPSHOT_LIST.replace("\r\n", "\n").trim());

        // Responses which don't contain a complete echoed line have no output.
        assert_eq!(parse_response(PROMPT_STR.as_bytes()), "");
        assert_eq!(parse_response(b""), "");
    }

    #[test]
    fn test_check_output() {
        assert_eq!(check_output(""), Ok(()));
        assert_eq!(check_output("There is no snapshot available."), Ok(()));
        assert_eq!(
            check_output("Error: Snapshot 'missing' does not exist in one or more devices"),
            Err("Snapshot 'missing' does not exist in one or more devices".to_string())
        );
        // Warnings may be printed before the error.
        assert_eq!(
            check_output("warning: something\nError: the reason it failed"),
            Err("the reason it failed".to_string())
        );
        // Warnings alone are not failures.
        assert_eq!(check_output("warning: something"), Ok(()));
    }

    #[test]
    fn test_parse_snapshot_list() {
        let output = SNAPSHOT_LIST.replace("\r\n", "\n");
        let snapshots = parse_snapshot_list(&output).expect("parse snapshots");
        assert_eq!(
            snapshots.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["booted", "second"]
        );
        assert_eq!(snapshots[0].vm_size, "382 MiB");
        assert_eq!(snapshots[0].date, "2026-09-30 22:30:24");
        assert_eq!(snapshots[0].vm_clock, "0000:00:19.615");
        assert_eq!(snapshots[1].vm_size, "1.45 GiB");
    }

    #[test]
    fn test_parse_snapshot_list_empty() {
        assert_eq!(parse_snapshot_list("There is no snapshot available.").unwrap(), vec![]);
        assert_eq!(parse_snapshot_list("").unwrap(), vec![]);
    }

    #[test]
    fn test_parse_snapshot_list_name_with_spaces() {
        // Snapshots which were saved directly through the monitor can have any name.
        let output = "List of snapshots present on all disks:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            --      my first snapshot  1.2 GiB 2026-09-30 22:30:24  0000:00:19.615         --\n";
        let snapshots = parse_snapshot_list(output).expect("parse snapshots");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "my first snapshot");
        assert_eq!(snapshots[0].vm_size, "1.2 GiB");
    }

    #[test]
    fn test_parse_snapshot_list_ignores_partial_snapshots() {
        // Snapshots which are not present on all of the disks cannot be loaded.
        let output = "List of snapshots present on all disks:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            --      booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --\n\
            \n\
            List of partial (non-loadable) snapshots on 'vdisk':\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            2       partial               0 B 2026-09-30 22:31:00  0000:00:25.000         --\n";
        let snapshots = parse_snapshot_list(output).expect("parse snapshots");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "booted");

        let output = "List of snapshots present on all disks:\n\
            None\n\
            \n\
            List of partial (non-loadable) snapshots on 'vdisk':\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            2       partial               0 B 2026-09-30 22:31:00  0000:00:25.000         --\n";
        assert_eq!(parse_snapshot_list(output).unwrap(), vec![]);
    }

    #[test]
    fn test_parse_snapshot_table() {
        // This is how `qemu-img snapshot -l` lists the snapshots in an image.
        let output = "Snapshot list:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            1       booted            382 MiB 2026-09-30 22:30:24  0000:00:19.615         --\n\
            2       second           1.45 GiB 2026-10-01 08:15:00  0001:02:03.004         --\n";
        let snapshots = parse_snapshot_table(output, "Snapshot list:").expect("parse snapshots");
        assert_eq!(
            snapshots,
            vec![
                SnapshotInfo {
                    name: "booted".to_string(),
                    vm_size: "382 MiB".to_string(),
                    date: "2026-09-30 22:30:24".to_string(),
                    vm_clock: "0000:00:19.615".to_string(),
                },
                SnapshotInfo {
                    name: "second".to_string(),
                    vm_size: "1.45 GiB".to_string(),
                    date: "2026-10-01 08:15:00".to_string(),
                    vm_clock: "0001:02:03.004".to_string(),
                },
            ]
        );

        // qemu-img prints nothing for an image which holds no snapshots.
        assert_eq!(parse_snapshot_table("", "Snapshot list:").unwrap(), vec![]);
    }

    #[test]
    fn test_parse_snapshot_list_unexpected_format() {
        let output = "List of snapshots present on all disks:\n\
            ID      TAG               VM_SIZE                DATE        VM_CLOCK     ICOUNT\n\
            this is not a snapshot\n";
        let err = parse_snapshot_list(output).expect_err("parse fails");
        assert!(err.to_string().contains("this is not a snapshot"), "{err}");
    }
}
