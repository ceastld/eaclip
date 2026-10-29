//! A silent, bounded video player. The decoder and its COM objects stay on one
//! worker thread; the popup only reads the current immutable frame.
use super::video::{Decoder, Frame};
use std::{
    path::PathBuf,
    sync::{mpsc, Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const MAX_WIDTH: u32 = 960;
const MAX_HEIGHT: u32 = 720;
const CANCEL_POLL: Duration = Duration::from_millis(100);
const DEFAULT_FRAME_DURATION: Duration = Duration::from_millis(33);

pub(super) struct Player {
    shared: Arc<Shared>,
}

#[derive(Clone)]
pub(super) struct Snapshot {
    pub frame: Arc<Frame>,
    pub playing: bool,
    pub ended: bool,
    pub error: Option<String>,
    pub revision: u64,
    pub duration: Option<Duration>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    frame: Option<Arc<Frame>>,
    duration: Option<Duration>,
    playing: bool,
    ended: bool,
    stopped: bool,
    rewind: bool,
    error: Option<String>,
    revision: u64,
    clock: PlaybackClock,
}

/// Pause records the actual media position when the click is received, even if
/// a synchronous decoder read is still finishing on the worker.
#[derive(Default)]
struct PlaybackClock {
    position: Duration,
    started: Option<Instant>,
}

impl PlaybackClock {
    fn position(&self, now: Instant) -> Duration {
        self.position
            .saturating_add(self.started.map_or(Duration::ZERO, |started| {
                now.saturating_duration_since(started)
            }))
    }

    fn play(&mut self, now: Instant) {
        if self.started.is_none() {
            self.started = Some(now);
        }
    }

    fn pause(&mut self, now: Instant) {
        self.position = self.position(now);
        self.started = None;
    }

    fn reset(&mut self, now: Instant, playing: bool) {
        self.position = Duration::ZERO;
        self.started = playing.then_some(now);
    }
}

impl State {
    fn new() -> Self {
        Self {
            frame: None,
            duration: None,
            playing: false,
            ended: false,
            stopped: false,
            rewind: false,
            error: None,
            revision: 0,
            clock: PlaybackClock::default(),
        }
    }

    fn stop(&mut self) {
        if !self.stopped {
            self.clock.pause(Instant::now());
            self.playing = false;
            self.stopped = true;
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn stop(&self) {
        self.state().stop();
        self.wake.notify_all();
    }

    fn fail(&self, error: String) {
        let mut state = self.state();
        if !state.stopped {
            state.clock.pause(Instant::now());
            state.playing = false;
            state.error = Some(error);
            state.revision = state.revision.wrapping_add(1);
        }
    }
}

impl Player {
    /// Blocking initialization is intended for the existing preview loader,
    /// never the native popup's UI thread. Only the first poster is decoded.
    pub fn open(
        path: PathBuf,
        active: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<Arc<Self>, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let (ready, initial) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("clip-hover-video".into())
            .spawn(move || worker(path, worker_shared, active, ready))
            .map_err(|error| format!("Could not start video preview: {error}"))?;
        initial
            .recv()
            .map_err(|_| "Video preview worker stopped during initialization".to_string())??;
        Ok(Arc::new(Self { shared }))
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.shared.state();
        Snapshot {
            // Player is only returned after its first frame has been installed.
            frame: Arc::clone(state.frame.as_ref().expect("initialized video poster")),
            playing: state.playing,
            ended: state.ended,
            error: state.error.clone(),
            revision: state.revision,
            duration: state.duration,
        }
    }

    /// The state changes immediately so the UI can redraw and start/stop its
    /// animation timer without waiting for decoding or seeking.
    pub fn toggle(&self) {
        let mut state = self.shared.state();
        if state.stopped || state.error.is_some() {
            return;
        }
        let now = Instant::now();
        if state.playing {
            state.clock.pause(now);
            state.playing = false;
        } else {
            if state.ended {
                state.ended = false;
                state.rewind = true;
                state.clock.reset(now, true);
            } else {
                state.clock.play(now);
            }
            state.playing = true;
        }
        state.revision = state.revision.wrapping_add(1);
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Cancellation never joins the decoder thread on the popup UI thread.
    pub fn stop(&self) {
        self.shared.stop();
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The worker owns Shared, not Player: its lifetime cannot keep playback
        // alive after the popup and all cached references release the player.
        self.shared.stop();
    }
}

fn worker(
    path: PathBuf,
    shared: Arc<Shared>,
    active: Arc<dyn Fn() -> bool + Send + Sync>,
    ready: mpsc::SyncSender<Result<(), String>>,
) {
    let initialized = (|| {
        if !active() {
            return Err("Video preview was cancelled".into());
        }
        let mut decoder = Decoder::open(&path, MAX_WIDTH, MAX_HEIGHT)?;
        if !active() {
            return Err("Video preview was cancelled".into());
        }
        let duration = decoder.info().duration;
        let frame = decoder
            .read_frame()?
            .ok_or_else(|| "Video contains no preview frames".to_string())?;
        if !active() {
            return Err("Video preview was cancelled".into());
        }
        Ok((decoder, frame, duration))
    })();
    let (mut decoder, poster, duration) = match initialized {
        Ok(initialized) => initialized,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let origin = poster.timestamp;
    {
        let mut state = shared.state();
        state.frame = Some(Arc::new(poster));
        state.duration = duration;
        state.revision = 1;
    }
    if ready.send(Ok(())).is_err() {
        return;
    }
    if let Err(error) = playback(&mut decoder, &shared, &*active, origin, duration) {
        shared.fail(error);
    }
}

enum Pending {
    Frame(Frame, Duration),
    End(Duration),
}

impl Pending {
    fn at(&self) -> Duration {
        match self {
            Self::Frame(_, at) | Self::End(at) => *at,
        }
    }
}

fn playback(
    decoder: &mut Decoder,
    shared: &Shared,
    active: &dyn Fn() -> bool,
    mut origin: Duration,
    duration: Option<Duration>,
) -> Result<(), String> {
    // Only the displayed frame and one decoded future frame are retained.
    let mut pending: Option<Pending> = None;
    let mut last_at = Duration::ZERO;
    let mut last_frame_duration = DEFAULT_FRAME_DURATION;
    loop {
        if !active() {
            shared.stop();
            return Ok(());
        }
        let mut state = shared.state();
        if state.stopped {
            return Ok(());
        }
        if state.rewind {
            state.rewind = false;
            drop(state);
            pending = None;
            decoder.rewind()?;
            let poster = decoder
                .read_frame()?
                .ok_or_else(|| "Video contains no preview frames".to_string())?;
            if !active() {
                shared.stop();
                return Ok(());
            }
            origin = poster.timestamp;
            last_at = Duration::ZERO;
            last_frame_duration = DEFAULT_FRAME_DURATION;
            let mut state = shared.state();
            if state.stopped {
                return Ok(());
            }
            state.frame = Some(Arc::new(poster));
            let playing = state.playing;
            state.clock.reset(Instant::now(), playing);
            state.revision = state.revision.wrapping_add(1);
            continue;
        }
        if !state.playing {
            drop(
                shared
                    .wake
                    .wait_timeout(state, CANCEL_POLL)
                    .unwrap_or_else(|error| error.into_inner()),
            );
            continue;
        }
        if let Some(next) = &pending {
            let now = Instant::now();
            let wait = next.at().saturating_sub(state.clock.position(now));
            if !wait.is_zero() {
                drop(
                    shared
                        .wake
                        .wait_timeout(state, wait.min(CANCEL_POLL))
                        .unwrap_or_else(|error| error.into_inner()),
                );
                continue;
            }
            match pending.take().expect("pending video event") {
                Pending::Frame(frame, at) => {
                    if at > last_at {
                        last_frame_duration = at - last_at;
                    }
                    last_at = at;
                    state.frame = Some(Arc::new(frame));
                }
                Pending::End(_) => {
                    state.clock.pause(now);
                    state.playing = false;
                    state.ended = true;
                }
            }
            state.revision = state.revision.wrapping_add(1);
            continue;
        }
        drop(state);
        // A pause or stop can arrive during this synchronous read. The result
        // stays pending until the next state check and is never shown early.
        pending = Some(match decoder.read_frame()? {
            Some(frame) => {
                let at = frame.timestamp.saturating_sub(origin).max(last_at);
                Pending::Frame(frame, at)
            }
            None => Pending::End(
                duration
                    .unwrap_or_else(|| last_at.saturating_add(last_frame_duration))
                    .max(last_at),
            ),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_clock_excludes_paused_time() {
        let start = Instant::now();
        let mut clock = PlaybackClock::default();
        clock.play(start);
        clock.pause(start + Duration::from_millis(125));
        assert_eq!(
            clock.position(start + Duration::from_secs(8)),
            Duration::from_millis(125)
        );
        clock.play(start + Duration::from_secs(8));
        assert_eq!(
            clock.position(start + Duration::from_millis(8090)),
            Duration::from_millis(215)
        );
    }

    #[test]
    fn replay_clock_starts_from_zero() {
        let start = Instant::now();
        let mut clock = PlaybackClock::default();
        clock.play(start);
        clock.pause(start + Duration::from_secs(4));
        clock.reset(start + Duration::from_secs(6), true);
        assert_eq!(
            clock.position(start + Duration::from_millis(6030)),
            Duration::from_millis(30)
        );
        clock.reset(start + Duration::from_secs(8), false);
        assert_eq!(
            clock.position(start + Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    fn test_player() -> Player {
        let mut state = State::new();
        state.frame = Some(Arc::new(Frame {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
            timestamp: Duration::ZERO,
        }));
        Player {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                wake: Condvar::new(),
            }),
        }
    }

    #[test]
    fn toggle_updates_the_ui_snapshot_immediately_and_replays_after_eos() {
        let player = test_player();
        assert!(!player.snapshot().playing);
        player.toggle();
        assert!(player.snapshot().playing);
        player.toggle();
        assert!(!player.snapshot().playing);
        {
            let mut state = player.shared.state();
            state.ended = true;
        }
        let poster = player.snapshot().frame;
        player.toggle();
        let snapshot = player.snapshot();
        assert!(snapshot.playing);
        assert!(!snapshot.ended);
        assert!(player.shared.state().rewind);
        assert!(Arc::ptr_eq(&poster, &snapshot.frame));
    }

    #[test]
    fn stop_and_last_player_drop_cancel_without_a_thread_join() {
        let player = test_player();
        player.toggle();
        player.stop();
        player.toggle();
        assert!(!player.snapshot().playing);
        assert!(player.shared.state().stopped);
        let shared = Arc::clone(&player.shared);
        drop(player);
        assert!(shared.state().stopped);

        let player = test_player();
        let shared = Arc::clone(&player.shared);
        drop(player);
        assert!(shared.state().stopped);
    }
}
