# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Scenarios for Starnix line discipline testing.
"""

SCENARIOS = [
    {
        "name": "canon_simple_echo",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "Hello\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "basic_backspace",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "He\x7flo\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "canon_word_erase",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world\x17\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "canon_kill_line",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "ignore me\x15keep\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "canon_echo_ctl",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "A\x01B\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "ixon_basic",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_slave", "data": "Hello\n"},
            {"action": "read_from_master", "data": "Hello\r\n"},
            # Stop output
            {"action": "write_to_master", "data": "\x13"},
            {"action": "sleep", "duration": 0.1},
            # Write more data, should be buffered but not visible.
            # Linux blocks immediately, so we expect blocking.
            {
                "action": "write_to_slave",
                "data": "World\n",
                "expect_block": True,
            },
            # Resuming output
            {"action": "write_to_master", "data": "\x11"},
            # Now write should succeed
            {"action": "write_to_slave", "data": "World\n"},
            # And we should read it
            {"action": "read_from_master", "data": "World\r\n"},
        ],
    },
    {
        "name": "echo_nl",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHONL"
                # ECHO is explicitly missing
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "a\n"},
            # 'a' is NOT echoed. '\n' IS echoed (as \r\n due to ONLCR).
            {"action": "read_from_master", "data": "\r\n"},
            {"action": "read_from_slave", "data": "a\n"},
        ],
    },
    {
        "name": "noflsh_sigint",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "NOFLSH",  # prevent flushing
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "foo"},
            {"action": "write_to_master", "data": "\x03"},  # ^C (SIGINT)
            {"action": "write_to_master", "data": "\n"},
            # 'foo' should be echoed. ^C echoed as ^C. \n echoed.
            {"action": "read_from_master", "data": "foo^C\r\n"},
            # 'foo' should NOT be discarded.
            {"action": "read_from_slave", "data": "foo\n"},
        ],
    },
    {
        "name": "echo_extended",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOE",
                "ECHOK",
                "ECHOCTL",
                "ECHOKE",
                "IEXTEN",
            ],
        },
        "events": [
            # ECHOCTL behavior
            {"action": "write_to_master", "data": "Ctl\x01\n"},
            {
                "action": "read_from_master",
                "data": "Ctl^A\r\n",
            },  # \x01 echoed as ^A
            {"action": "read_from_slave", "data": "Ctl\x01\n"},
            # ECHOE behavior (already tested in basic_backspace, but double check)
            {"action": "write_to_master", "data": "Erase\x7f\n"},
            {
                "action": "read_from_master",
                "data": "Erase\x08 \x08\r\n",
            },  # BS SP BS
            {"action": "read_from_slave", "data": "Eras\n"},
        ],
    },
    {
        "name": "echo_prt",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                # ECHOPRT overrides ECHOE/ECHOKE usually or tries to use it.
                "ECHOPRT",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "a\x7f\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echo_prt_two_chars",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                # ECHOPRT overrides ECHOE/ECHOKE usually or tries to use it.
                "ECHOPRT",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "aa\x7f\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world\x17\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase_typing",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abc def\x17ghi\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_kill",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "ignore me\x15keep\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echonl_echoprt",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": ["ISIG", "ICANON", "ECHONL", "ECHOPRT", "IEXTEN"],
        },
        "events": [
            {"action": "write_to_master", "data": "a"},
            {"action": "write_to_master", "data": "\x7f"},
            {"action": "write_to_master", "data": "b\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase_space",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world"},
            # Ctrl+W
            {"action": "write_to_master", "data": "\x17"},
            # Space
            {"action": "write_to_master", "data": " "},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase_tab",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world"},
            # Ctrl+W
            {"action": "write_to_master", "data": "\x17"},
            # Tab
            {"action": "write_to_master", "data": "\t"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase_nl_aln",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world"},
            # Ctrl+W
            {"action": "write_to_master", "data": "\x17"},
            # Newline + 'a'
            {"action": "write_to_master", "data": "\na"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_word_erase_nl_nl",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world"},
            # Ctrl+W
            {"action": "write_to_master", "data": "\x17"},
            # Newline + Newline
            {"action": "write_to_master", "data": "\n\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_kill_nl_aln",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "hello world"},
            # Ctrl+U (KILL)
            {"action": "write_to_master", "data": "\x15"},
            # Newline + 'a'
            {"action": "write_to_master", "data": "\na"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_multi_aln",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Alphanumeric
            {"action": "write_to_master", "data": "xy"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_multi_space",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Space
            {"action": "write_to_master", "data": " "},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_multi_nl",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Newline
            {"action": "write_to_master", "data": "\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_nl_aln",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Newline + Alphanumeric
            {"action": "write_to_master", "data": "\n"},
            {"action": "write_to_master", "data": "a"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_nl_space",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Newline + Space
            {"action": "write_to_master", "data": "\n"},
            {"action": "write_to_master", "data": " "},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "echoprt_backspace_nl_nl",
        "initial_termios": {
            "c_iflag": ["ICRNL", "IXON"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": [
                "ISIG",
                "ICANON",
                "ECHO",
                "ECHOPRT",
                "ECHOK",
                "IEXTEN",
            ],
        },
        "events": [
            {"action": "write_to_master", "data": "abcde"},
            # 3 Backspaces
            {"action": "write_to_master", "data": "\x7f\x7f\x7f"},
            # Newline + Newline
            {"action": "write_to_master", "data": "\n\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
]

INPUT_SCENARIOS = [
    {
        "name": "input_igncr",
        "initial_termios": {
            "c_iflag": ["IGNCR"],
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": ["ICANON", "ECHO"],  # Simplest echo to verify
        },
        "events": [
            {"action": "write_to_master", "data": "a\rb\n"},
            # \r should be ignored. a, b, \n should be echoed.
            # \n echoed as \r\n
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "input_inlcr",
        "initial_termios": {
            "c_iflag": ["INLCR"],
            "c_oflag": ["OPOST"],  # No ONLCR to avoid confusion
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            {"action": "write_to_master", "data": "a\nb\n"},
            {"action": "write_to_master", "data": "\x04"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "input_icrnl_prec",
        "initial_termios": {
            "c_iflag": ["IGNCR", "ICRNL"],  # IGNCR should take precedence
            "c_oflag": ["OPOST", "ONLCR"],
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            {"action": "write_to_master", "data": "a\rb\n"},
            # \r ignored (IGNCR). It does NOT become \n (ICRNL).
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
]

OUTPUT_SCENARIOS = [
    {
        "name": "output_ocrnl",
        "initial_termios": {
            "c_iflag": ["ICRNL"],
            "c_oflag": ["OPOST", "OCRNL"],
            # OCRNL: Map CR to NL on output.
            # NOTE: ONLCR (NL->CRNL) default is usually on.
            # If ONLCR is ALSO on, then CR -> NL, and NL -> CRNL!
            # Wait, ONLCR affects NL. OCRNL affects CR.
            # If I send CR, and OCRNL is set, it becomes NL.
            # Then if ONLCR is set, does that NL become CRNL?
            # POSIX says "CR characters are converted to NL."
            # It does NOT say this NL is then subject to ONLCR. Usually these are separate transformations.
            # Let's verify Linux behavior.
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            {"action": "write_to_master", "data": "a\r"},  # Input CR.
            # ICRNL: \r -> \n on input.
            # Echo: \n is echoed.
            # Output processing of echoed char:
            # If \n is echoed, ONLCR might apply?
            # Test logic: write to master -> PTY master -> tty line discipline input -> echo -> output processing -> PTY master read.
            # Wait, if I write to master it goes to input queue.
            # Input queue (ICRNL) converts \r to \n.
            # Echo sees \n.
            # Output queue sees \n.
            # ONLCR (if set) converts \n to \r\n.
            # OCRNL affects *output* CR.
            # To test OCRNL, we need the *output* stream to contain a CR.
            # Echoing a \r would do it (if ICRNL is OFF).
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        # Explicit test for OCRNL where we produce a CR in output by avoiding ICRNL
        "name": "output_ocrnl_explicit",
        "initial_termios": {
            "c_iflag": [],  # No ICRNL
            "c_oflag": ["OPOST", "OCRNL"],  # Map Output CR -> NL
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            {"action": "write_to_master", "data": "a\r"},
            # \r input -> \r echoed.
            # Output processing: \r -> \n (OCRNL).
            # Expect 'a', '\n'.
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "output_onocr",
        "initial_termios": {
            "c_iflag": [],
            "c_oflag": ["OPOST", "ONOCR"],  # Don't output CR at column 0
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            # At column 0 (start).
            {"action": "write_to_master", "data": "\r"},
            # Should NOT echo anything (suppressed).
            {"action": "write_to_master", "data": "a\r"},
            # 'a' (col 1), then \r (goes to col 0). Echoed.
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "output_onlret",
        "initial_termios": {
            "c_iflag": [],
            "c_oflag": ["OPOST", "ONLRET"],  # NL performs CR function
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            # ONLRET means NL is assumed to return to col 0.
            # This mostly affects column tracking for tabs?
            # Starnix implementation:
            # if output_flags & ONLRET: column = 0 (on \n)
            # Does it affect the *bytes* sent?
            # "The NL character is assumed to do the carriage-return function; the column pointer will be set to 0. ONLCR is not implemented." (Linux man)
            # Doesn't change bytes, just state.
            # To verify state, we need a TAB.
            # TAB behavior depends on column.
            {"action": "write_to_master", "data": "a\n"},
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
            {"action": "write_to_master", "data": "\t"},
            # 'a' (col 1). '\n' (col 0 because ONLRET).
            # '\t' should advance to next tab stop (8).
            # If ONLRET was NOT set, '\n' might not reset column?
            # Wait, '\n' normally moves down, not left.
            # So state column would stay at 1?
            # If column stays at 1, '\t' moves to 8.
            # If column resets to 0, '\t' moves to 8.
            # Wait tab stops are fixed (8, 16...).
            # If col is 1, next tab is 8. Added spaces: 7.
            # If col is 0, next tab is 8. Added spaces: 8.
            # SO bytes output will differ!
            # If ONLRET is set, we expect 8 spaces after \n.
            # If ONLRET is NOT set (and no ONLCR outputting \r), column remains?
            # Linux: \n is just LF. Column?
            # Usually LF doesn't change column.
            # So 'a' (1) -> LF -> col 1. Tab -> 7 spaces.
            # With ONLRET: 'a' (1) -> LF -> col 0. Tab -> 8 spaces.
            # WE need XTABS to see spaces, otherwise we just see \t.
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
    {
        "name": "output_xtabs",
        "initial_termios": {
            "c_iflag": [],
            "c_oflag": ["OPOST", "XTABS"],  # Expand tabs to spaces
            "c_lflag": ["ICANON", "ECHO"],
        },
        "events": [
            {"action": "write_to_master", "data": "\t"},
            # 8 spaces.
            {"action": "write_to_master", "data": "a\t"},
            # 'a' + 7 spaces.
            {"action": "read_from_master"},
            {"action": "read_from_slave"},
        ],
    },
]

ALL_SCENARIOS = {}
for s in SCENARIOS:
    ALL_SCENARIOS[s["name"]] = s
for s in INPUT_SCENARIOS:
    ALL_SCENARIOS[s["name"]] = s
for s in OUTPUT_SCENARIOS:
    ALL_SCENARIOS[s["name"]] = s
