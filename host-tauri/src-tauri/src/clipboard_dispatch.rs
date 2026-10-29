//! Keep native clipboard ownership on one message-pumping thread. Clipboard
//! contention must not occupy the Tauri window thread.
use super::windows_api as windows;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_APP, WM_QUIT,
};

const WAKE: u32 = WM_APP + 113;
const STOPPED: &str = "Clipboard operation cancelled: the application is shutting down.";
type Job = Box<dyn FnOnce() -> Result<(), String> + Send>;

struct Request {
    job: Job,
    reply: mpsc::SyncSender<Result<(), String>>,
}

struct State {
    accepting: bool,
    stopping: bool,
    requests: VecDeque<Request>,
}

pub struct ClipboardDispatcher {
    state: Arc<Mutex<State>>,
    thread_id: u32,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ClipboardDispatcher {
    pub fn start() -> Result<Self, String> {
        let state = Arc::new(Mutex::new(State {
            accepting: true,
            stopping: false,
            requests: VecDeque::new(),
        }));
        let worker_state = state.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("cliphost-clipboard-write".into())
            .spawn(move || pump(worker_state, ready_tx))
            .map_err(|error| error.to_string())?;
        let thread_id = match ready_rx.recv() {
            Ok(id) => id,
            Err(error) => {
                let _ = worker.join();
                return Err(format!("Clipboard thread initialization failed: {error}"));
            }
        };
        Ok(Self {
            state,
            thread_id,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn run(&self, job: Job) -> Result<(), String> {
        if unsafe { GetCurrentThreadId() } == self.thread_id {
            return Err("Clipboard dispatcher cannot wait on its own thread.".into());
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let mut state = self.state.lock().unwrap();
            if !state.accepting {
                return Err(STOPPED.into());
            }
            state.requests.push_back(Request {
                job,
                reply: reply_tx,
            });
            // Posting cannot dispatch the job synchronously. Retaining this lock
            // lets a failed post withdraw exactly this request before it runs.
            if let Err(error) =
                unsafe { PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0)) }
            {
                state.requests.pop_back();
                return Err(format!("Clipboard write dispatch failed: {error}"));
            }
        }
        reply_rx
            .recv()
            .unwrap_or_else(|error| Err(format!("Clipboard write thread stopped: {error}")))
    }

    pub fn shutdown(&self) {
        let cancelled = {
            let mut state = self.state.lock().unwrap();
            state.accepting = false;
            state.stopping = true;
            let cancelled = std::mem::take(&mut state.requests);
            let _ = unsafe { PostThreadMessageW(self.thread_id, WAKE, WPARAM(0), LPARAM(0)) };
            cancelled
        };
        cancel(cancelled, STOPPED);
        if unsafe { GetCurrentThreadId() } == self.thread_id {
            return;
        }
        if let Some(worker) = self.worker.lock().unwrap().take() {
            if worker.join().is_err() {
                tracing::error!("Clipboard write thread panicked during shutdown");
            }
        }
    }
}

impl Drop for ClipboardDispatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn cancel(requests: VecDeque<Request>, reason: &str) {
    for request in requests {
        let _ = request.reply.send(Err(reason.to_owned()));
    }
}

fn pump(state: Arc<Mutex<State>>, ready: mpsc::SyncSender<u32>) {
    let mut message = MSG::default();
    // PostThreadMessage requires the target's queue to exist before publishing
    // its ID. No clipboard owner HWND exists until the first native write.
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
    if ready.send(unsafe { GetCurrentThreadId() }).is_err() {
        return;
    }
    let reason = 'work: loop {
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT {
                break 'work STOPPED.to_owned();
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // Always recheck after pumping: PeekMessage may have consumed a wake
        // posted by a request arriving while another message was dispatched.
        let request = {
            let mut pending = state.lock().unwrap();
            if pending.stopping {
                break STOPPED.to_owned();
            }
            pending.requests.pop_front()
        };
        if let Some(request) = request {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(request.job))
                .unwrap_or_else(|_| Err("Clipboard write task panicked.".into()));
            let pending = state.lock().unwrap();
            // A completed write must not let its caller inject paste input
            // after shutdown has cancelled the rest of the operation.
            let result = if pending.stopping {
                Err(STOPPED.into())
            } else {
                result
            };
            let _ = request.reply.send(result);
            continue;
        }
        // Empty queue: block without periodic polling, while Windows can still
        // dispatch sent messages to this thread's message-only owner window.
        let received = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if received == 0 {
            break STOPPED.to_owned();
        }
        if received < 0 {
            break format!(
                "Clipboard message loop failed: {}",
                windows::core::Error::from_win32()
            );
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    };
    let cancelled = {
        let mut pending = state.lock().unwrap();
        pending.accepting = false;
        pending.stopping = true;
        std::mem::take(&mut pending.requests)
    };
    cancel(cancelled, &reason);
    // Returning runs clipboard_payload's thread-local Owner::drop here, so its
    // HWND is destroyed by the same thread that created it.
}

#[cfg(test)]
mod tests {
    use super::windows;
    use super::*;
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HWND, LRESULT};
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::w;

    const PROBE: u32 = WM_APP + 114;
    #[derive(Default)]
    struct Probe {
        calls: AtomicUsize,
        dispatched_on: AtomicU32,
        destroyed_on: AtomicU32,
    }
    struct Owner {
        hwnd: HWND,
        _probe: Arc<Probe>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
    thread_local! { static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) }; }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let probe = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe;
        if !probe.is_null() {
            if message == PROBE {
                (*probe).calls.fetch_add(1, Ordering::SeqCst);
                (*probe)
                    .dispatched_on
                    .store(GetCurrentThreadId(), Ordering::SeqCst);
                return LRESULT(73);
            }
            if message == WM_DESTROY {
                (*probe)
                    .destroyed_on
                    .store(GetCurrentThreadId(), Ordering::SeqCst);
            }
        }
        DefWindowProcW(hwnd, message, w, l)
    }

    fn owner(probe: Arc<Probe>) -> Owner {
        static CLASS: OnceLock<()> = OnceLock::new();
        CLASS.get_or_init(|| unsafe {
            assert_ne!(
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(window_proc),
                    lpszClassName: w!("ClipHostClipboardDispatcherTest"),
                    ..Default::default()
                }),
                0
            );
        });
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("ClipHostClipboardDispatcherTest"),
                w!("Clipboard dispatcher probe"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                Some(Arc::as_ptr(&probe) as *const _),
            )
            .unwrap()
        };
        Owner {
            hwnd,
            _probe: probe,
        }
    }

    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "dispatcher fixture did not reach the expected state"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn blocked_write_keeps_caller_window_responsive_and_preserves_order_and_errors() {
        let service = Arc::new(ClipboardDispatcher::start().unwrap());
        let probe = Arc::new(Probe::default());
        let ui = owner(probe.clone());
        let caller_id = unsafe { GetCurrentThreadId() };
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_service = service.clone();
        let first_order = order.clone();
        let first = thread::spawn(move || {
            first_service.run(Box::new(move || {
                first_order.lock().unwrap().push(1);
                started_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
                release_rx.recv().map_err(|error| error.to_string())?;
                Ok(())
            }))
        });
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            service.thread_id
        );
        assert_ne!(service.thread_id, caller_id);
        unsafe {
            PostMessageW(ui.hwnd, PROBE, WPARAM(0), LPARAM(0)).unwrap();
        }
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.dispatched_on.load(Ordering::SeqCst), caller_id);
        let second_service = service.clone();
        let second_order = order.clone();
        let second = thread::spawn(move || {
            second_service.run(Box::new(move || {
                second_order.lock().unwrap().push(2);
                Err("original clipboard error".into())
            }))
        });
        wait(|| service.state.lock().unwrap().requests.len() == 1);
        assert_eq!(*order.lock().unwrap(), vec![1]);
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(()));
        assert_eq!(
            second.join().unwrap(),
            Err("original clipboard error".into())
        );
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        service.shutdown();
    }

    #[test]
    fn idle_owner_pumps_messages_and_shutdown_cancels_pending_and_destroys_on_owner_thread() {
        let service = Arc::new(ClipboardDispatcher::start().unwrap());
        let probe = Arc::new(Probe::default());
        let hwnd = Arc::new(AtomicIsize::new(0));
        let worker_probe = probe.clone();
        let worker_hwnd = hwnd.clone();
        service
            .run(Box::new(move || {
                let window = owner(worker_probe);
                worker_hwnd.store(window.hwnd.0 as isize, Ordering::SeqCst);
                OWNER.with(|slot| *slot.borrow_mut() = Some(window));
                Ok(())
            }))
            .unwrap();
        let mut reply = 0;
        let delivered = unsafe {
            SendMessageTimeoutW(
                HWND(hwnd.load(Ordering::SeqCst) as *mut _),
                PROBE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                2000,
                Some(&mut reply),
            )
        };
        assert_ne!(
            delivered.0, 0,
            "idle worker did not dispatch its owner message"
        );
        assert_eq!(reply, 73);
        assert_eq!(
            probe.dispatched_on.load(Ordering::SeqCst),
            service.thread_id
        );
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let active_service = service.clone();
        let active = thread::spawn(move || {
            active_service.run(Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().map_err(|error| error.to_string())?;
                Ok(())
            }))
        });
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let executed = Arc::new(AtomicUsize::new(0));
        let cancelled_executed = executed.clone();
        let pending_service = service.clone();
        let pending = thread::spawn(move || {
            pending_service.run(Box::new(move || {
                cancelled_executed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
        });
        wait(|| service.state.lock().unwrap().requests.len() == 1);
        let stopping_service = service.clone();
        let stopped = thread::spawn(move || stopping_service.shutdown());
        wait(|| service.state.lock().unwrap().stopping);
        assert_eq!(pending.join().unwrap(), Err(STOPPED.into()));
        assert_eq!(service.run(Box::new(|| Ok(()))), Err(STOPPED.into()));
        release_tx.send(()).unwrap();
        assert_eq!(active.join().unwrap(), Err(STOPPED.into()));
        stopped.join().unwrap();
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.destroyed_on.load(Ordering::SeqCst), service.thread_id);
        service.shutdown();
    }
}
