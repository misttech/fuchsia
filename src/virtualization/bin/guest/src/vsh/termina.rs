// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use fidl_fuchsia_virtualization::{
    ContainerStatus, LinuxGuestInfo, LinuxManagerEvent, LinuxManagerProxy,
};
use fuchsia_async::{Interval, MonotonicDuration};

use futures::future::ready;
use futures::{StreamExt, select, stream};
use std::io::Write;

// ANSI Escape sequences for terminal manipulation.
// see: https://en.wikipedia.org/wiki/ANSI_escape_code#CSI_(Control_Sequence_Introducer)_sequences
macro_rules! csi {
    ( $( $cmd:expr ),* ) => {
        concat!("\x1b[", $($cmd),*)
    }
}

const CURSOR_HIDE: &str = csi!("?", "25", "l");
const CURSOR_SHOW: &str = csi!("?", "25", "h");
const COLOUR0_NORMAL: &str = csi!("0", "m");
const COLOUR1_RED_BRIGHT: &str = csi!("1;31", "m");
const COLOUR2_GREEN_BRIGHT: &str = csi!("1;32", "m");
const COLOUR3_YELLOW: &str = csi!("33", "m");
const COLOUR5_MAGENTA: &str = csi!("35", "m");
const ERASE_REST_OF_LINE: &str = csi!("K");

fn move_forward(cells: usize) -> String {
    format!(csi!("{}", "C"), cells)
}

// This function maps ContainerStatus to arbitrary progress markers from 1 - 10
const fn get_container_status_progress(status: ContainerStatus) -> usize {
    match status {
        ContainerStatus::Transient | ContainerStatus::LaunchingGuest => 1,
        ContainerStatus::StartingVm => 2,
        ContainerStatus::Downloading => 4,
        ContainerStatus::Extracting => 6,
        ContainerStatus::Starting => 9,
        ContainerStatus::Failed | ContainerStatus::Ready => 10,
    }
}

const MAX_CONTAINER_STATUS_PROGRESS: usize = get_container_status_progress(ContainerStatus::Ready);

// This function defines the message to print for each stage of startup.
fn get_container_status_string(info: &LinuxGuestInfo) -> String {
    match info.container_status.expect("LinuxGuestInfo should contain a container_status") {
        ContainerStatus::Transient => String::new(),
        ContainerStatus::LaunchingGuest => "Initializing".to_string(),
        ContainerStatus::StartingVm => "Starting the virtual machine".to_string(),
        ContainerStatus::Downloading => {
            format!(
                "Downloading the Linux container image ({}%)",
                info.download_percent.expect("LinuxGuestInfo should contain a download_percent")
            )
        }
        ContainerStatus::Extracting => "Extracting the Linux container image".to_string(),
        ContainerStatus::Starting => "Starting the Linux container".to_string(),
        ContainerStatus::Ready => "Ready".to_string(),
        ContainerStatus::Failed => format!("Error starting guest: {:?}", info.failure_reason),
    }
}

// Print initial progress bar and hide the cursor.
fn print_progress_bar(w: &mut impl Write) -> Result<()> {
    let padding_width = MAX_CONTAINER_STATUS_PROGRESS;
    write!(w, "{CURSOR_HIDE}{COLOUR5_MAGENTA}[{:padding_width$}]", "")?;
    Ok(())
}

// Print the ContainerStatus string to the right of the rendered progress bar. The offset of the
// new end of line is returned so that sub-messages can be printed.
fn print_stage(
    w: &mut impl Write,
    colour: &str,
    status: ContainerStatus,
    output: &str,
) -> Result<usize> {
    let status_progress = get_container_status_progress(status);
    let progress_bar: String = "=".chars().cycle().take(status_progress).collect();
    let forward = move_forward(3 + (MAX_CONTAINER_STATUS_PROGRESS - status_progress));
    write!(w, "\r{COLOUR5_MAGENTA}[{progress_bar}{forward}{ERASE_REST_OF_LINE}{colour}{output}")?;

    // Return the offset of the end of line position
    Ok(4 + MAX_CONTAINER_STATUS_PROGRESS + output.len())
}

// Prints a message to the right of the progress bar and the last "stage" message printed.
fn print_after_stage(
    w: &mut impl Write,
    end_of_line: usize,
    colour: &str,
    output: &str,
) -> Result<()> {
    let forward = move_forward(end_of_line);
    write!(w, "\r{forward}{colour}: {output}")?;
    Ok(())
}

/// Launches a Termina VM. An ascii progress bar and the current status info are rendered to `w`
/// until the launch sequence terminates.
pub async fn launch(linux_manager: &LinuxManagerProxy, w: &mut impl Write) -> Result<()> {
    const TERMINA_ENVIRONMENT_NAME: &str = "termina";

    let linux_guest_info = linux_manager
        .start_and_get_linux_guest_info(TERMINA_ENVIRONMENT_NAME)
        .await?
        .map_err(zx::Status::from_raw)?;
    let mut info = linux_guest_info.clone();

    // Add the first event to the same stream as the subsequent ones to simplify handling
    let mut events = stream::once(ready(Ok(LinuxManagerEvent::OnGuestInfoChanged {
        label: TERMINA_ENVIRONMENT_NAME.to_string(),
        info: linux_guest_info,
    })))
    .chain(linux_manager.take_event_stream());

    print_progress_bar(w)?;

    let mut spinner = "|/-\\".chars().cycle();
    let mut end_of_line = 0;
    let mut interval = Interval::new(MonotonicDuration::from_millis(100));
    let final_info = loop {
        info = select! {
            () = interval.select_next_some() => {
                let progress = get_container_status_progress(
                    info.container_status
                        .expect("LinuxGuestInfo should contain a container_status"),
                );
                write!(w,
                    "\r{}{}{}",
                    move_forward(progress),
                    COLOUR5_MAGENTA,
                    spinner.next().expect("Infinite iterator should not terminate")
                )?;
                info
            }
            maybe_event = events.next() => {
                let event = maybe_event
                    .context("LinuxManagerEvent stream unexpectedly terminated")?
                    .context("LinuxManagerEvent stream encountered a fidl error")?;
                let LinuxManagerEvent::OnGuestInfoChanged { label, info } = event;
                if &label != TERMINA_ENVIRONMENT_NAME {
                    continue;
                }

                log::debug!("LinuxManagerEvent: {:?}", info);

                let container_status = if let Some(status) = info.container_status {
                    status
                } else {
                    break info;
                };

                let stage_text = get_container_status_string(&info);
                end_of_line = match container_status {
                    ContainerStatus::Failed => {
                        print_after_stage(w, end_of_line, COLOUR1_RED_BRIGHT, &stage_text)?;
                        write!(w, "\r\n{ERASE_REST_OF_LINE}{COLOUR0_NORMAL}{CURSOR_SHOW}")?;
                        break info;
                    }
                    ContainerStatus::Ready => {
                        print_stage(w, COLOUR2_GREEN_BRIGHT, container_status, &stage_text)?;
                        write!(w, "\r\n{ERASE_REST_OF_LINE}{COLOUR0_NORMAL}{CURSOR_SHOW}")?;
                        break info;
                    }
                    _ => print_stage(w, COLOUR3_YELLOW, container_status, &stage_text)?,
                };

                info
            }
        };

        // Don't necessarily care too much about being unable to flush.
        w.flush().ok();
    };

    w.flush().ok();

    match final_info.container_status {
        Some(ContainerStatus::Ready) => {}
        Some(ContainerStatus::Failed) => anyhow::bail!("Container failed to start"),
        None => anyhow::bail!("container_status unexpectedly missing!"),
        _ => unreachable!(),
    };

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use vt100::Parser;

    fn get_text(parser: &Parser) -> Vec<String> {
        let screen = parser.screen();
        let (num_rows, num_cols) = screen.size();
        let mut res = Vec::new();
        for r in 0..num_rows {
            let mut s = String::new();
            for c in 0..num_cols {
                if let Some(cell) = screen.cell(r, c) {
                    if cell.has_contents() {
                        s.push_str(cell.contents());
                    } else {
                        s.push(' ');
                    }
                } else {
                    s.push(' ');
                }
            }
            res.push(s);
        }
        res
    }

    #[test]
    fn test_move_forward() {
        let mut term = Parser::new(24, 80, 0);

        term.process(move_forward(40).as_bytes());

        assert_eq!(term.screen().cursor_position(), (0, 40));
        assert_eq!(get_text(&term), vec![" ".repeat(80); 24]);
    }

    #[test]
    fn test_erase_in_line() {
        let mut term = Parser::new(1, 20, 0);

        term.process(b"0123456789");
        assert_eq!(get_text(&term), vec!["0123456789          "]);

        term.process(format!("\r{}{}", move_forward(5), ERASE_REST_OF_LINE).as_bytes());
        assert_eq!(get_text(&term), vec!["01234               "]);
    }

    #[test]
    fn test_cursor_hide_and_show() {
        let mut term = Parser::new(1, 20, 0);

        assert!(!term.screen().hide_cursor());

        term.process(CURSOR_HIDE.as_bytes());
        assert!(term.screen().hide_cursor());

        term.process(CURSOR_SHOW.as_bytes());
        assert!(!term.screen().hide_cursor());

        assert_eq!(get_text(&term), vec![" ".repeat(20); 1]);
    }

    #[test]
    fn test_colours() {
        let mut term = Parser::new(1, 12, 0);

        term.process(
            format!(
                "0{}12{}345{}6{}7{}8{}9 ",
                COLOUR3_YELLOW,
                COLOUR1_RED_BRIGHT,
                COLOUR0_NORMAL,
                COLOUR5_MAGENTA,
                COLOUR2_GREEN_BRIGHT,
                COLOUR5_MAGENTA,
            )
            .as_bytes(),
        );

        let screen = term.screen();
        let mut cells = vec![];
        for i in 0..12 {
            cells.push(screen.cell(0, i).unwrap());
        }

        assert_eq!(cells[0].contents(), "0");
        assert_eq!(cells[0].fgcolor(), vt100::Color::Default);

        assert_eq!(cells[1].contents(), "1");
        assert_eq!(cells[1].fgcolor(), vt100::Color::Idx(3));

        assert_eq!(cells[2].contents(), "2");
        assert_eq!(cells[2].fgcolor(), vt100::Color::Idx(3));

        assert_eq!(cells[3].contents(), "3");
        assert_eq!(cells[3].fgcolor(), vt100::Color::Idx(1));
        assert!(cells[3].bold());

        assert_eq!(cells[4].contents(), "4");
        assert_eq!(cells[4].fgcolor(), vt100::Color::Idx(1));
        assert!(cells[4].bold());

        assert_eq!(cells[5].contents(), "5");
        assert_eq!(cells[5].fgcolor(), vt100::Color::Idx(1));
        assert!(cells[5].bold());

        assert_eq!(cells[6].contents(), "6");
        assert_eq!(cells[6].fgcolor(), vt100::Color::Default);
        assert!(!cells[6].bold());

        assert_eq!(cells[7].contents(), "7");
        assert_eq!(cells[7].fgcolor(), vt100::Color::Idx(5));
        assert!(!cells[7].bold());

        assert_eq!(cells[8].contents(), "8");
        assert_eq!(cells[8].fgcolor(), vt100::Color::Idx(2));
        assert!(cells[8].bold());

        assert_eq!(cells[9].contents(), "9");
        assert_eq!(cells[9].fgcolor(), vt100::Color::Idx(5));
        assert!(cells[9].bold());

        assert_eq!(cells[10].contents(), " ");
        assert_eq!(cells[10].fgcolor(), vt100::Color::Idx(5));
        assert!(cells[10].bold());

        assert_eq!(cells[11].contents(), "");
        assert_eq!(cells[11].fgcolor(), vt100::Color::Default);
        assert!(!cells[11].bold());
    }

    #[test]
    fn test_progress_bar() {
        let mut term = Parser::new(5, 20, 0);
        let mut input = vec![];
        print_progress_bar(&mut input).unwrap();
        term.process(&input);

        assert!(term.screen().hide_cursor());
        assert_eq!(
            get_text(&term),
            vec![
                "[          ]        ",
                "                    ",
                "                    ",
                "                    ",
                "                    ",
            ]
        );

        let screen = term.screen();
        for i in 0..12 {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Idx(5));
        }
        for i in 12..20 {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Default);
        }
    }

    #[test]
    fn test_print_stage() {
        let mut term = Parser::new(2, 50, 0);

        let mut input = vec![];
        print_progress_bar(&mut input).unwrap();
        term.process(&input);

        let mut input = vec![];
        let download_msg = "Some download message";
        let line_end =
            print_stage(&mut input, COLOUR3_YELLOW, ContainerStatus::Downloading, download_msg)
                .unwrap();
        term.process(&input);

        let mut input = vec![];
        let after_msg = "Err details";
        print_after_stage(&mut input, line_end, COLOUR5_MAGENTA, after_msg).unwrap();
        term.process(&input);

        assert_eq!(
            get_text(&term),
            vec![
                "[====      ]  Some download message: Err details  ",
                "                                                  ",
            ]
        );

        let screen = term.screen();
        for i in 0..12 {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Idx(5));
        }
        for i in 12..14 {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Default);
        }
        for i in 14..(14 + download_msg.len() as u16) {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Idx(3));
        }
        for i in (14 + download_msg.len() as u16)
            ..(14 + download_msg.len() as u16 + after_msg.len() as u16 + 2)
        {
            assert_eq!(screen.cell(0, i).unwrap().fgcolor(), vt100::Color::Idx(5));
        }
    }
}
