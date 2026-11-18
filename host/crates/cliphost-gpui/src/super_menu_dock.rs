//! 2026-10-31 top-edge Super menu dock, replacing the file line as the
//! top-edge entry. Rest the pointer at the top screen edge and the Super menu
//! opens docked: its category cards under the pointer, each opening its menu
//! on hover (`SuperMenuWindow`). Timing reuses the file line's edge trigger.

use std::{sync::Arc, time::{Duration, Instant}};

use cliphost_core::{host::HostState, screen_edge};
use gpui::{App, AppContext, Context, Entity, Task, Timer};

use crate::{
    file_line_trigger::{Change, EdgeTrigger, Sample},
    platform,
};

const POLL: Duration = Duration::from_millis(25);
/// While the setting is off, only notice when it turns on.
const IDLE_POLL: Duration = Duration::from_millis(500);
/// Logical pixels under the edge that still count as resting there.
const PEEK_BAND: f64 = 30.0;

pub struct SuperMenuDock {
    host: Arc<HostState>,
    trigger: EdgeTrigger,
    /// After a docked opening, the pointer must leave the edge before the
    /// next one, so closing the menu at the edge does not reopen it.
    armed: bool,
    _poll: Task<()>,
}

impl SuperMenuDock {
    pub fn start(host: Arc<HostState>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let poll = cx.spawn(async move |this, cx| loop {
                let Ok(delay) = this.update(cx, |dock, _| dock.tick()) else { break };
                Timer::after(delay).await;
            });
            Self { host, trigger: EdgeTrigger::default(), armed: true, _poll: poll }
        })
    }

    fn tick(&mut self) -> Duration {
        if !self.host.super_menu_dock_enabled() {
            self.trigger.retract();
            return IDLE_POLL;
        }
        let Some((x, y)) = platform::cursor_position() else { return POLL };
        let Some(edge) = screen_edge::screen_edge_from_point(x, y) else { return POLL };
        let within = f64::from(x) >= edge.monitor.left && f64::from(x) < edge.monitor.left + edge.monitor.width;
        let at_edge = within && f64::from(y) <= edge.monitor.top + 1.0;
        if !at_edge {
            self.armed = true;
        }
        if !self.armed || self.host.super_menu_active() {
            self.trigger.retract();
            return POLL;
        }
        let in_peek_band = at_edge || within && (f64::from(y) - edge.monitor.top) / edge.dpi_scale <= PEEK_BAND;
        // Only ask about full-screen programs when the edge could trigger.
        let blocked = at_edge && screen_edge::foreground_is_fullscreen();
        self.trigger.set_delay(self.host.super_menu_dock_delay());
        let sample = Sample { now: Instant::now(), x, at_edge, in_peek_band, in_line: false, blocked };
        if self.trigger.step(sample) == Change::Drop {
            self.trigger.retract();
            self.armed = false;
            if let Err(error) = self.host.request_super_menu_docked() {
                tracing::debug!(%error, "Top-edge Super menu unavailable");
            }
        }
        POLL
    }
}
