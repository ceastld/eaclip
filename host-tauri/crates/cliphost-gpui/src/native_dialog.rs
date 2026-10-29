//! Shell pickers need an STA. GPUI's Windows background executor uses the
//! WinRT MTA thread pool, so each modal operation gets a fresh native thread.
//! Core owns COM initialization and releases its dialog before that STA ends.

const WORKER_STOPPED: &str = "The native dialog worker stopped before returning a result.";

fn worker_start_error(error: std::io::Error) -> String {
    format!("Failed to start the native dialog worker: {error}")
}

pub(crate) async fn run<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (reply, result) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("clip-native-dialog".into())
        .spawn(move || {
            let _ = reply.send(operation());
        })
        .map_err(worker_start_error)?;
    result.await.map_err(|_| WORKER_STOPPED.to_string())?
}

/// For an existing background task only. The scoped thread permits borrowing
/// its host/request while keeping the modal and its COM lifetime off the MTA.
pub(crate) fn run_blocking<T: Send>(
    operation: impl FnOnce() -> Result<T, String> + Send,
) -> Result<T, String> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("clip-native-dialog".into())
            .spawn_scoped(scope, operation)
            .map_err(worker_start_error)?
            .join()
            .map_err(|_| WORKER_STOPPED.to_string())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    #[tokio::test]
    async fn preserves_cancelled_selections_and_operation_errors() {
        assert_eq!(run(|| Ok(None::<String>)).await, Ok(None));
        assert_eq!(
            run(|| Err::<(), _>("selection failed".into())).await,
            Err("selection failed".into()),
        );
        let borrowed = "selection failed".to_string();
        assert_eq!(run_blocking(|| Ok(None::<String>)), Ok(None));
        assert_eq!(
            run_blocking(|| Err::<(), _>(borrowed.clone())),
            Err(borrowed),
        );
    }

    #[tokio::test]
    async fn waiting_for_a_dialog_does_not_block_the_calling_thread() {
        let caller = std::thread::current().id();
        let (started, starting) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let mut result = std::pin::pin!(run(move || {
            assert_ne!(std::thread::current().id(), caller);
            let _ = started.send(());
            waiting.recv().map_err(|error| error.to_string())?;
            Ok("selected")
        }));
        assert!(matches!(
            result
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), starting)
            .await
            .expect("native dialog worker did not start")
            .unwrap();
        assert!(matches!(
            result
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending,
        ));
        release.send(()).unwrap();
        assert_eq!(result.await, Ok("selected"));
    }

    #[cfg(windows)]
    #[test]
    fn mta_caller_can_run_a_picker_in_a_separate_sta() {
        use crate::windows_api::Win32::System::Com::{
            CoGetApartmentType, CoInitializeEx, CoUninitialize, APTTYPE, APTTYPEQUALIFIER,
            APTTYPE_MAINSTA, APTTYPE_MTA, APTTYPE_STA, COINIT_APARTMENTTHREADED,
            COINIT_MULTITHREADED,
        };

        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe {
                    CoUninitialize();
                }
            }
        }

        std::thread::spawn(|| unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let _caller_apartment = Apartment;
            let mut kind = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            CoGetApartmentType(&mut kind, &mut qualifier).unwrap();
            assert_eq!(kind, APTTYPE_MTA);
            assert!(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err());

            let caller = std::thread::current().id();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let worker = runtime
                .block_on(run(move || {
                    CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                        .ok()
                        .map_err(|error| error.to_string())?;
                    let _picker_apartment = Apartment;
                    let mut kind = APTTYPE::default();
                    let mut qualifier = APTTYPEQUALIFIER::default();
                    CoGetApartmentType(&mut kind, &mut qualifier)
                        .map_err(|error| error.to_string())?;
                    assert!(kind == APTTYPE_STA || kind == APTTYPE_MAINSTA);
                    Ok(std::thread::current().id())
                }))
                .unwrap();
            assert_ne!(worker, caller);
            let borrowed = caller;
            let worker = run_blocking(|| {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|error| error.to_string())?;
                let _picker_apartment = Apartment;
                assert_ne!(std::thread::current().id(), borrowed);
                Ok(std::thread::current().id())
            })
            .unwrap();
            assert_ne!(worker, caller);
            CoGetApartmentType(&mut kind, &mut qualifier).unwrap();
            assert_eq!(
                kind, APTTYPE_MTA,
                "the caller keeps its original COM apartment"
            );
        })
        .join()
        .unwrap();
    }
}
