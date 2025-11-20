# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import sys
from typing import Any, Optional

_logger: Optional[logging.Logger] = None


_EXCEPTION = logging.CRITICAL + 1


class ColoredFormatter(logging.Formatter):
    """A logging formatter that adds colors to the output."""

    COLORS = {
        "DEBUG": "\033[94m",  # Blue
        "INFO": "\033[92m",  # Green
        "WARNING": "\033[93m",  # Yellow
        "ERROR": "\033[91m",  # Red
        "CRITICAL": "\033[1;91m",  # Bold Red
    }
    RESET = "\033[0m"

    def __init__(self, fmt: str) -> None:
        super().__init__(fmt)

    def format(self, record: logging.LogRecord) -> str:
        log_message = super().format(record)
        # Don't colorize exception messages.
        if record.exc_info:
            return log_message
        color = self.COLORS.get(record.levelname)
        if color:
            return f"{color}{log_message}{self.RESET}"
        return log_message


def init_logger(level: int = logging.INFO, colors: bool = False) -> None:
    """
    Initializes the global logger.

    Args:
        level: The minimum log level to display.
        colors: Whether to use colored output.
    """
    global _logger
    logger = logging.getLogger("cog")

    # Clear any existing handlers. This can happen if log() is called before
    # init_logger(), which causes may cause duplicate output if we do not clear
    # the old logger handler.
    logger.handlers.clear()

    logger.setLevel(level)
    handler = logging.StreamHandler(sys.stdout)

    formatter: logging.Formatter
    if colors:
        formatter = ColoredFormatter("%(levelname)s: %(message)s")
    else:
        formatter = logging.Formatter("%(levelname)s: %(message)s")

    handler.setFormatter(formatter)
    logger.addHandler(handler)
    _logger = logger


def log(level: int, *args: Any, **kwargs: Any) -> None:
    """
    Logs a message with the given level.

    This function has an API similar to the print function.

    Args:
        level: The log level (e.g., logging.INFO, 'INFO', 'DEBUG').
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.

    Raises:
        Exception: If the logger has not been initialized.
        ValueError: If the log level is invalid.
    """
    if _logger is None:
        init_logger(logging.WARNING, colors=False)

    # Needed to make mypy happy
    assert _logger

    sep = kwargs.get("sep", " ")
    message = sep.join(map(str, args))

    if level == _EXCEPTION:
        _logger.exception(message)
        return

    _logger.log(level, message)


def log_info(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level INFO.

    This function has an API similar to the print function.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(logging.INFO, *args, **kwargs)


def log_debug(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level DEBUG.

    This function has an API similar to the print function.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(logging.DEBUG, *args, **kwargs)


def log_warn(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level WARNING.

    This function has an API similar to the print function.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(logging.WARNING, *args, **kwargs)


def log_error(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level ERROR.

    This function has an API similar to the print function.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(logging.ERROR, *args, **kwargs)


def log_critical(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level CRITICAL.

    This function has an API similar to the print function.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(logging.CRITICAL, *args, **kwargs)


def log_exception(*args: Any, **kwargs: Any) -> None:
    """
    Logs a message with level ERROR, including exception information.

    This function has an API similar to the print function and should be called
    from within an exception handler.

    Args:
        *args: The message parts to log.
        **kwargs: Supports 'sep' to specify a separator.
    """
    log(_EXCEPTION, *args, **kwargs)
