//! Shared retirement boundary for device recovery and swap-chain resizing.
//!
//! A failed build leaves an explicit empty slot. Neither a later retry nor
//! normal destruction can release the retired resource a second time.

pub(super) fn rebuild<T, E>(
    slot: &mut Option<T>,
    build: impl FnOnce() -> Result<T, E>,
) -> Result<(), E> {
    drop(slot.take());
    *slot = Some(build()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rebuild;
    use std::{cell::RefCell, rc::Rc};

    struct Resource {
        id: usize,
        releases: Rc<RefCell<Vec<usize>>>,
    }

    impl Drop for Resource {
        fn drop(&mut self) {
            self.releases.borrow_mut().push(self.id);
        }
    }

    fn resource(id: usize, releases: &Rc<RefCell<Vec<usize>>>) -> Resource {
        Resource {
            id,
            releases: releases.clone(),
        }
    }

    #[test]
    fn every_recovery_failure_stage_releases_each_resource_once_then_allows_retry() {
        // Device, surface, globals, pipelines and composition are independently
        // fallible. Exercise every partial-construction boundary, repeatedly.
        for fail_at in 0..=5 {
            let releases = Rc::new(RefCell::new(Vec::new()));
            let mut slot = Some(vec![resource(0, &releases)]);
            for attempt in 0..3 {
                let result = rebuild(&mut slot, || {
                    assert_eq!(releases.borrow().iter().filter(|id| **id == 0).count(), 1);
                    let mut candidate = Vec::new();
                    for stage in 0..5 {
                        if stage == fail_at {
                            return Err("injected allocation failure");
                        }
                        candidate.push(resource(1 + attempt * 5 + stage, &releases));
                    }
                    if fail_at == 5 {
                        return Err("injected final attach failure");
                    }
                    Ok(candidate)
                });
                assert!(result.is_err());
                assert!(slot.is_none());
            }
            rebuild(&mut slot, || Ok::<_, ()>(vec![resource(100, &releases)])).unwrap();
            drop(slot);
            let released = releases.borrow();
            let mut unique = released.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(
                unique.len(),
                released.len(),
                "a COM ownership was released twice"
            );
            assert_eq!(released.len(), 2 + 3 * fail_at.min(5));
            assert!(released.contains(&100));
        }
    }

    #[test]
    fn failed_resize_and_partial_surface_build_leave_no_stale_target() {
        let releases = Rc::new(RefCell::new(Vec::new()));
        let mut target = Some(vec![resource(0, &releases)]);
        // ResizeBuffers may fail before a new texture can be acquired.
        assert!(rebuild(&mut target, || Err::<Vec<Resource>, _>("ResizeBuffers")).is_err());
        assert!(target.is_none());
        // A retry may obtain a texture, then fail creating its views.
        assert!(
            rebuild(&mut target, || {
                let _texture = resource(1, &releases);
                Err::<Vec<Resource>, _>("CreateRenderTargetView")
            })
            .is_err()
        );
        assert!(target.is_none());
        rebuild(&mut target, || Ok::<_, ()>(vec![resource(2, &releases)])).unwrap();
        drop(target);
        assert_eq!(*releases.borrow(), vec![0, 1, 2]);
    }

    #[test]
    fn destroying_after_all_recovery_attempts_fail_does_not_release_retired_state_again() {
        let releases = Rc::new(RefCell::new(Vec::new()));
        let mut slot = Some(resource(0, &releases));
        for _ in 0..5 {
            assert!(rebuild(&mut slot, || Err::<Resource, _>("device unavailable")).is_err());
        }
        drop(slot);
        assert_eq!(*releases.borrow(), vec![0]);
    }
}
