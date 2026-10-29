//! The native UI sleeps until actual work arrives. Only visible interactions,
//! posted HWND operations and finite resource deadlines need a timer.

use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Notify};

pub(super) fn next_delay(
    active: Option<Duration>,
    deadlines: impl IntoIterator<Item = Instant>,
    now: Instant,
) -> Option<Duration> {
    active.into_iter().chain(deadlines.into_iter().map(|at| at.saturating_duration_since(now))).min()
}

/// Both queues and Notify::notify_one retain work arriving between the state
/// snapshot and this wait. Losing a select branch never consumes another
/// broadcast event; a command notification is only a coalesced scheduling hint.
pub(super) async fn wait_for_work<T: Clone>(
    events: &mut broadcast::Receiver<T>,
    chrome: &Notify,
    local: &Notify,
    delay: Option<Duration>,
) -> Result<Option<T>, broadcast::error::RecvError> {
    tokio::select! {
        event = events.recv() => event.map(Some),
        _ = chrome.notified() => Ok(None),
        _ = local.notified() => Ok(None),
        _ = async {
            match delay {
                Some(delay) => gpui::Timer::after(delay).await,
                None => std::future::pending().await,
            }
        } => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_idle_has_no_timer_and_deadlines_remain_exact() {
        let now = Instant::now();
        assert_eq!(next_delay(None, [], now), None);
        assert_eq!(next_delay(None, [now + Duration::from_secs(180), now + Duration::from_secs(30)], now), Some(Duration::from_secs(30)));
        assert_eq!(next_delay(Some(Duration::from_millis(50)), [now + Duration::from_secs(30)], now), Some(Duration::from_millis(50)));
        assert_eq!(next_delay(None, [now - Duration::from_millis(1)], now), Some(Duration::ZERO));
    }

    #[tokio::test]
    async fn hidden_wait_blocks_and_a_queued_show_wakes_without_a_timer() {
        let (_sender, mut events) = broadcast::channel::<()>(8);
        let chrome = Notify::new();
        let local = Notify::new();
        assert!(tokio::time::timeout(Duration::from_millis(20), wait_for_work(&mut events, &chrome, &local, None)).await.is_err());
        // Show can arrive before the waiting future is registered.
        chrome.notify_one();
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), wait_for_work(&mut events, &chrome, &local, None)).await.unwrap().unwrap(), None);
        assert!(tokio::time::timeout(Duration::from_millis(20), wait_for_work(&mut events, &chrome, &local, None)).await.is_err());
    }

    #[tokio::test]
    async fn simultaneous_event_and_command_keep_the_event() {
        let (sender, mut events) = broadcast::channel(8);
        let chrome = Notify::new();
        let local = Notify::new();
        sender.send("clipboard changed").unwrap();
        chrome.notify_one();
        let first = wait_for_work(&mut events, &chrome, &local, None).await.unwrap();
        let event = match first {
            Some(event) => event,
            None => wait_for_work(&mut events, &chrome, &local, None).await.unwrap().unwrap(),
        };
        assert_eq!(event, "clipboard changed");
    }

    #[tokio::test]
    async fn local_completion_wakes_and_closed_service_is_terminal() {
        let (sender, mut events) = broadcast::channel::<()>(8);
        let chrome = Notify::new();
        let local = Notify::new();
        local.notify_one();
        assert_eq!(wait_for_work(&mut events, &chrome, &local, None).await.unwrap(), None);
        drop(sender);
        assert_eq!(wait_for_work(&mut events, &chrome, &local, None).await, Err(broadcast::error::RecvError::Closed));
    }
}
