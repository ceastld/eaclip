use super::*;
use std::cell::Cell;

const OWNER: isize = 17;
const PROCESS: u32 = 42;
const VALID: OwnerSnapshot = OwnerSnapshot {
    root: OWNER,
    process_id: PROCESS,
    thread_id: 7,
};

#[test]
fn rejects_zero_missing_child_foreign_and_threadless_owners() {
    for (case, owner, state) in [
        ("zero", 0, Some(OwnerSnapshot { root: 0, ..VALID })),
        ("missing", OWNER, None),
        ("child", OWNER, Some(OwnerSnapshot { root: 18, ..VALID })),
        (
            "foreign",
            OWNER,
            Some(OwnerSnapshot {
                process_id: PROCESS + 1,
                ..VALID
            }),
        ),
        (
            "threadless",
            OWNER,
            Some(OwnerSnapshot {
                thread_id: 0,
                ..VALID
            }),
        ),
    ] {
        assert!(
            owner_guard(
                owner,
                PROCESS,
                || true,
                |probed| {
                    assert_eq!(probed, owner);
                    state
                }
            )
            .is_err(),
            "{case}"
        );
    }
}

#[test]
fn captured_root_identity_can_be_checked_repeatedly() {
    let probes = Cell::new(0);
    let guard = owner_guard(
        OWNER,
        PROCESS,
        || true,
        |owner| {
            assert_eq!(owner, OWNER);
            probes.set(probes.get() + 1);
            Some(VALID)
        },
    )
    .ok()
    .expect("valid root must bind");
    assert_eq!(probes.get(), 1);
    for _ in 0..4 {
        assert_eq!(guard(), Ok(()));
    }
    assert_eq!(probes.get(), 5);
}

#[test]
fn rejects_changed_root_process_thread_or_destroyed_owner() {
    for replacement in [
        None,
        Some(OwnerSnapshot { root: 18, ..VALID }),
        Some(OwnerSnapshot {
            process_id: PROCESS + 1,
            ..VALID
        }),
        Some(OwnerSnapshot {
            thread_id: 8,
            ..VALID
        }),
        Some(OwnerSnapshot {
            thread_id: 0,
            ..VALID
        }),
    ] {
        let state = Cell::new(Some(VALID));
        let guard = owner_guard(OWNER, PROCESS, || true, |_| state.get())
            .ok()
            .expect("original root must bind");
        assert_eq!(guard(), Ok(()));
        state.set(replacement);
        assert!(guard().is_err(), "{replacement:?}");
    }
}

#[test]
fn retired_lease_at_capture_does_not_probe_the_owner() {
    let probes = Cell::new(0);
    let result = owner_guard(
        OWNER,
        PROCESS,
        || false,
        |_| {
            probes.set(probes.get() + 1);
            Some(VALID)
        },
    );
    assert!(result.is_err());
    assert_eq!(probes.get(), 0);
}

#[test]
fn lease_retired_inside_capture_probe_cannot_bind() {
    let active = Cell::new(true);
    let probes = Cell::new(0);
    let result = owner_guard(
        OWNER,
        PROCESS,
        || active.get(),
        |_| {
            probes.set(probes.get() + 1);
            active.set(false);
            Some(VALID)
        },
    );
    assert!(result.is_err());
    assert_eq!(probes.get(), 1);
}

#[test]
fn lease_retired_before_a_check_skips_the_owner_probe() {
    let active = Cell::new(true);
    let probes = Cell::new(0);
    let guard = owner_guard(
        OWNER,
        PROCESS,
        || active.get(),
        |_| {
            probes.set(probes.get() + 1);
            Some(VALID)
        },
    )
    .ok()
    .expect("original root must bind");
    active.set(false);
    assert!(guard().is_err());
    assert_eq!(probes.get(), 1);
}

#[test]
fn lease_retired_during_a_check_probe_rejects_the_same_identity() {
    let active = Cell::new(true);
    let probes = Cell::new(0);
    let guard = owner_guard(
        OWNER,
        PROCESS,
        || active.get(),
        |_| {
            probes.set(probes.get() + 1);
            if probes.get() == 2 {
                active.set(false);
            }
            Some(VALID)
        },
    )
    .ok()
    .expect("original root must bind");
    assert!(guard().is_err());
    assert_eq!(probes.get(), 2);
}

#[test]
fn retired_one_way_lease_blocks_reused_hwnd_with_identical_identity() {
    let active = Cell::new(true);
    let state = Cell::new(Some(VALID));
    let probes = Cell::new(0);
    let guard = owner_guard(
        OWNER,
        PROCESS,
        || active.get(),
        |_| {
            probes.set(probes.get() + 1);
            state.get()
        },
    )
    .ok()
    .expect("original root must bind");
    assert_eq!(guard(), Ok(()));

    // Inject destruction and a same-PID/thread/root replacement. The caller's
    // original request lease stays retired even when every HWND field matches.
    active.set(false);
    state.set(None);
    state.set(Some(VALID));
    assert!(guard().is_err());
    assert!(guard().is_err());
    assert_eq!(probes.get(), 2);
}

#[test]
fn owned_picker_keeps_its_original_root_and_preserves_cancel_and_errors() {
    for outcome in [Ok(Vec::<String>::new()), Err("folder picker failed".to_string())] {
        let calls = Cell::new(0);
        let result = run_with_owner(
            OWNER,
            PROCESS,
            || true,
            |owner| {
                assert_eq!(owner, OWNER, "never replace the captured owner");
                Some(VALID)
            },
            |verify| {
                calls.set(calls.get() + 1);
                verify()?;
                outcome.clone()
            },
        );
        assert_eq!(result, outcome);
        assert_eq!(calls.get(), 1);
    }
}

#[test]
fn owned_picker_retirement_prevents_start_and_rejects_selected_paths() {
    let started = Cell::new(false);
    let result = run_with_owner(
        OWNER,
        PROCESS,
        || false,
        |_| panic!("a retired request must not probe live windows"),
        |_| {
            started.set(true);
            Ok(Vec::<String>::new())
        },
    );
    assert!(result.is_err());
    assert!(!started.get());

    let active = Cell::new(true);
    let result = run_with_owner(
        OWNER,
        PROCESS,
        || active.get(),
        |_| Some(VALID),
        |verify| {
            verify()?;
            active.set(false);
            Ok(vec!["C:\\picked-folder".to_string()])
        },
    );
    assert_eq!(result, Err(retired()));
}
