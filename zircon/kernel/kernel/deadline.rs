// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::{
    ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST, ZX_TIMER_SLACK_CENTER, ZX_TIMER_SLACK_EARLY,
    ZX_TIMER_SLACK_LATE,
};

pub use crate::platform_rs::timer::{DurationBoot, DurationMono, DurationUnknown, InstantUnknown};

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackMode {
    /// slack is centered around deadline
    Center = ZX_TIMER_SLACK_CENTER,
    /// slack interval is (deadline - slack, deadline]
    Early = ZX_TIMER_SLACK_EARLY,
    // slack interval is [deadline, deadline + slack)
    Late = ZX_TIMER_SLACK_LATE,
}

/// TimerSlack specifies how much a timer or event is allowed to deviate from its deadline.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerSlack {
    amount: DurationUnknown,
    mode: SlackMode,
}

impl TimerSlack {
    /// Used to indicate that a given deadline is not eligible for coalescing.
    ///
    /// Not intended to be used for timers/events that originate on behalf of usermode.
    pub const fn none() -> Self {
        Self { amount: DurationUnknown(0), mode: SlackMode::Center }
    }

    /// Create a TimerSlack object with the specified |amount| and |mode|.
    ///
    /// |amount| must be >= 0. 0 means "no slack" (i.e. no coalescing is allowed).
    pub const fn new(amount: DurationUnknown, mode: SlackMode) -> Self {
        debug_assert!(amount.0 >= 0);
        Self { amount, mode }
    }

    pub const fn amount(&self) -> DurationUnknown {
        self.amount
    }

    pub const fn mode(&self) -> SlackMode {
        self.mode
    }
}

/// Deadline specifies when a timer or event should occur.
///
/// This class encapsulates the point in time at which a timer/event should occur ("when") and how
/// much the timer/event is allowed to deviate from that point in time ("slack"). The point in time
/// can be on the boot or monotonic clock.
///
/// TODO(https://fxbug.dev/319935985): The fact that this class does not encapsulate the timeline
/// the deadline is on is a footgun. Callers should be careful when passing deadlines around to
/// ensure that the proper timeline is always used.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    when: InstantUnknown,
    slack: TimerSlack,
}

impl Deadline {
    pub const fn new(when: InstantUnknown, slack: TimerSlack) -> Self {
        Self { when, slack }
    }

    pub const fn when(&self) -> InstantUnknown {
        self.when
    }

    pub const fn slack(&self) -> TimerSlack {
        self.slack
    }

    pub const fn no_slack(when: InstantUnknown) -> Self {
        Self { when, slack: TimerSlack::none() }
    }

    /// A deadline that will never be reached.
    pub const fn infinite() -> Self {
        Self { when: InstantUnknown(ZX_TIME_INFINITE), slack: TimerSlack::none() }
    }

    /// A deadline that's always in the past.
    pub const fn infinite_past() -> Self {
        Self { when: InstantUnknown(ZX_TIME_INFINITE_PAST), slack: TimerSlack::none() }
    }

    /// Construct a monotonic deadline using relative duration measured from now.
    pub fn after_mono(after: DurationMono, slack: TimerSlack) -> Self {
        let now = crate::platform_rs::timer::current_mono_time();
        Self { when: InstantUnknown(now.0.saturating_add(after.0)), slack }
    }

    /// Construct a boot deadline using relative duration measured from now.
    pub fn after_boot(after: DurationBoot, slack: TimerSlack) -> Self {
        let now = crate::platform_rs::timer::current_boot_time();
        Self { when: InstantUnknown(now.0.saturating_add(after.0)), slack }
    }

    /// Returns the earliest point in time at which this deadline may occur.
    pub const fn earliest(&self) -> InstantUnknown {
        match self.slack.mode {
            SlackMode::Center | SlackMode::Early => {
                InstantUnknown(self.when.0.saturating_sub(self.slack.amount.0))
            }
            SlackMode::Late => self.when,
        }
    }

    /// Returns the latest point in time at which this deadline may occur.
    pub const fn latest(&self) -> InstantUnknown {
        match self.slack.mode {
            SlackMode::Center | SlackMode::Late => {
                InstantUnknown(self.when.0.saturating_add(self.slack.amount.0))
            }
            SlackMode::Early => self.when,
        }
    }
}

zr::static_assert!(core::mem::size_of::<SlackMode>() == 4);
zr::static_assert!(core::mem::align_of::<SlackMode>() == 4);
zr::static_assert!(core::mem::size_of::<TimerSlack>() == 16);
zr::static_assert!(core::mem::align_of::<TimerSlack>() == 8);
zr::static_assert!(core::mem::offset_of!(TimerSlack, amount) == 0);
zr::static_assert!(core::mem::offset_of!(TimerSlack, mode) == 8);
zr::static_assert!(core::mem::size_of::<Deadline>() == 24);
zr::static_assert!(core::mem::align_of::<Deadline>() == 8);
zr::static_assert!(core::mem::offset_of!(Deadline, when) == 0);
zr::static_assert!(core::mem::offset_of!(Deadline, slack) == 8);

/// FFI trampoline for [`Deadline::earliest`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_deadline_earliest(deadline: &Deadline) -> zx_types::zx_time_t {
    deadline.earliest().0
}

/// FFI trampoline for [`Deadline::latest`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_deadline_latest(deadline: &Deadline) -> zx_types::zx_time_t {
    deadline.latest().0
}

/// Unit tests for [`Deadline`].
#[cfg(ktest)]
#[unittest::suite(name = "deadline_rust")]
mod tests {
    use unittest::expect_eq;

    /// Tests `earliest` and `latest` across `Center`, `Early`, `Late`, and `no_slack` modes.
    #[test]
    fn earliest_and_latest_slack_modes() {
        let when = InstantUnknown(1000);
        let amount = DurationUnknown(200);

        let center = Deadline::new(when, TimerSlack::new(amount, SlackMode::Center));
        expect_eq!(center.earliest().0, 800);
        expect_eq!(center.latest().0, 1200);

        let early = Deadline::new(when, TimerSlack::new(amount, SlackMode::Early));
        expect_eq!(early.earliest().0, 800);
        expect_eq!(early.latest().0, 1000);

        let late = Deadline::new(when, TimerSlack::new(amount, SlackMode::Late));
        expect_eq!(late.earliest().0, 1000);
        expect_eq!(late.latest().0, 1200);

        let no_slack = Deadline::no_slack(when);
        expect_eq!(no_slack.earliest().0, when.0);
        expect_eq!(no_slack.latest().0, when.0);
    }

    /// Tests saturation at `ZX_TIME_INFINITE` and `ZX_TIME_INFINITE_PAST`.
    #[test]
    fn earliest_and_latest_saturation() {
        let inf = Deadline::new(
            InstantUnknown(ZX_TIME_INFINITE),
            TimerSlack::new(DurationUnknown(100), SlackMode::Center),
        );
        expect_eq!(inf.latest().0, ZX_TIME_INFINITE);
        expect_eq!(inf.earliest().0, ZX_TIME_INFINITE - 100);

        let inf_past = Deadline::new(
            InstantUnknown(ZX_TIME_INFINITE_PAST),
            TimerSlack::new(DurationUnknown(100), SlackMode::Center),
        );
        expect_eq!(inf_past.earliest().0, ZX_TIME_INFINITE_PAST);
        expect_eq!(inf_past.latest().0, ZX_TIME_INFINITE_PAST + 100);
    }
}
