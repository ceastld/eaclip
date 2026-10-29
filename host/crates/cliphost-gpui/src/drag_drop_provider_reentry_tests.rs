//! Reentrant COM callbacks against our registered, off-screen root target.
//! Each case owns its provider, window, admission capture and drag-over lease.
use super::*;
use std::cell::Cell;

type Captures = Rc<RefCell<Vec<DropEvent>>>;
type Lease = Rc<RefCell<Option<DropTicket>>>;

fn capture_binding(root: &Root) -> (DragDropWindow, Captures, Lease, Rc<Cell<usize>>) {
    let captures = Rc::new(RefCell::new(Vec::new()));
    let received = captures.clone();
    let lease = Rc::new(RefCell::new(None::<DropTicket>));
    let dropping = lease.clone();
    let dragging = lease.clone();
    let errors = Rc::new(Cell::new(0));
    let failures = errors.clone();
    let binding = DragDropWindow::attach_stamped(
        root.0 .0 as isize,
        Rc::new(move |event| {
            assert!(dropping
                .borrow()
                .as_ref()
                .unwrap()
                .same_request(&event.ticket));
            received.borrow_mut().push(event);
            Ok(())
        }),
        Rc::new(move |event| match event {
            DragOverEvent::Enter(ticket) => *dragging.borrow_mut() = Some(ticket),
            DragOverEvent::Leave(ticket) => {
                let released = dragging.borrow_mut().take().unwrap();
                assert!(released.same_request(&ticket));
            }
        }),
        Rc::new(move |_| failures.set(failures.get() + 1)),
    )
    .unwrap();
    binding.sync_session(19, true);
    (binding, captures, lease, errors)
}

#[test]
fn owned_provider_query_leave_cannot_revive_the_enter_candidate() {
    std::thread::spawn(|| {
        let root = Root::new();
        let (_binding, captures, lease, errors) = capture_binding(&root);
        let target = root.target();
        let querying_target = target.clone();
        let fired = Rc::new(Cell::new(false));
        let querying = fired.clone();
        let (source, reads) = provider_with_query(
            vec![(13, wide("left while negotiating"))],
            Rc::new(move || {
                if !querying.replace(true) {
                    unsafe { querying_target.DragLeave().unwrap() };
                }
            }),
        );
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(fired.get(), "the actual provider QueryGetData must run");
        assert!(lease.borrow().is_none());
        assert!(reads.borrow().is_empty());
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(captures.borrow().is_empty());
        assert!(reads.borrow().is_empty());
        assert_eq!(errors.get(), 0);

        let (next, _) = provider(vec![(13, wide("next source"))], None);
        assert_eq!(enter(&target, &next, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(drop_at(&target, &next, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(captures.borrow().len(), 1);
        assert!(lease.borrow().is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn owned_provider_query_enter_cannot_overwrite_the_reentrant_source() {
    std::thread::spawn(|| {
        let root = Root::new();
        let (_binding, captures, lease, errors) = capture_binding(&root);
        let target = root.target();
        let (current, current_reads) = provider(vec![(13, wide("query replacement B"))], None);
        let querying_target = target.clone();
        let replacement = current.clone();
        let fired = Rc::new(Cell::new(false));
        let querying = fired.clone();
        let (old, old_reads) = provider_with_query(
            vec![(13, wide("query candidate A"))],
            Rc::new(move || {
                if !querying.replace(true) {
                    assert_eq!(
                        enter(&querying_target, &replacement, DROPEFFECT_COPY),
                        DROPEFFECT_COPY
                    );
                }
            }),
        );
        assert_eq!(enter(&target, &old, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(fired.get());
        let current_ticket = lease.borrow().as_ref().unwrap().clone();
        assert!(current_ticket.is_current());
        assert_eq!(drop_at(&target, &old, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(old_reads.borrow().is_empty());
        assert!(captures.borrow().is_empty());
        assert!(lease
            .borrow()
            .as_ref()
            .unwrap()
            .same_request(&current_ticket));
        assert_eq!(errors.get(), 0, "A must not stamp errors as B");
        assert_eq!(drop_at(&target, &current, DROPEFFECT_COPY), DROPEFFECT_COPY);
        let accepted = captures.borrow_mut().pop().unwrap();
        assert!(accepted.ticket.same_request(&current_ticket));
        assert_eq!(
            accepted.formats.text.as_deref(),
            Some("query replacement B")
        );
        assert_eq!(current_reads.borrow().len(), 1);
        assert!(lease.borrow().is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn owned_provider_getdata_leave_cannot_accept_the_captured_source() {
    std::thread::spawn(|| {
        let root = Root::new();
        let (_binding, captures, lease, errors) = capture_binding(&root);
        let target = root.target();
        let reading_target = target.clone();
        let fired = Rc::new(Cell::new(false));
        let reading = fired.clone();
        let (source, reads) = provider(
            vec![(13, wide("left while reading"))],
            Some(Rc::new(move || {
                if !reading.replace(true) {
                    unsafe { reading_target.DragLeave().unwrap() };
                }
            })),
        );
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        let entered = lease.borrow().as_ref().unwrap().clone();
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(fired.get(), "the actual provider GetData must run");
        assert_eq!(&*reads.borrow(), &[13]);
        assert!(captures.borrow().is_empty());
        assert!(lease.borrow().is_none());
        assert_eq!(errors.get(), 0, "Leave must suppress stale UI feedback");

        let (next, _) = provider(vec![(13, wide("after read cancellation"))], None);
        assert_eq!(enter(&target, &next, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert!(!entered.is_current());
        assert_eq!(drop_at(&target, &next, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(captures.borrow().len(), 1);
        assert!(lease.borrow().is_none());
    })
    .join()
    .unwrap();
}

#[test]
fn owned_provider_leave_observer_enter_cannot_be_overwritten_by_outer_enter() {
    std::thread::spawn(|| {
        let root = Root::new();
        let captures = Rc::new(RefCell::new(Vec::new()));
        let received = captures.clone();
        let lease = Rc::new(RefCell::new(None::<DropTicket>));
        let dropping = lease.clone();
        let dragging = lease.clone();
        let errors = Rc::new(Cell::new(0));
        let failures = errors.clone();
        let (replacement, replacement_reads) =
            provider(vec![(13, wide("observer replacement B"))], None);
        let reentrant_source = replacement.clone();
        let target_slot = Rc::new(RefCell::new(None::<Rc<IDropTarget>>));
        let observer_target = target_slot.clone();
        let fired = Rc::new(Cell::new(false));
        let observer = fired.clone();
        let binding = DragDropWindow::attach_stamped(
            root.0 .0 as isize,
            Rc::new(move |event| {
                assert!(dropping
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .same_request(&event.ticket));
                received.borrow_mut().push(event);
                Ok(())
            }),
            Rc::new(move |event| match event {
                DragOverEvent::Enter(ticket) => *dragging.borrow_mut() = Some(ticket),
                DragOverEvent::Leave(ticket) => {
                    let released = dragging.borrow_mut().take().unwrap();
                    assert!(released.same_request(&ticket));
                    if !observer.replace(true) {
                        let target = observer_target.borrow().as_ref().unwrap().clone();
                        assert_eq!(
                            enter(&target, &reentrant_source, DROPEFFECT_COPY),
                            DROPEFFECT_COPY
                        );
                    }
                }
            }),
            Rc::new(move |_| failures.set(failures.get() + 1)),
        )
        .unwrap();
        binding.sync_session(23, true);
        let target = Rc::new(root.target());
        *target_slot.borrow_mut() = Some(target.clone());
        let (original, original_reads) = provider(vec![(13, wide("original A"))], None);
        let (outer, outer_reads) = provider(vec![(13, wide("outer candidate C"))], None);
        assert_eq!(enter(&target, &original, DROPEFFECT_COPY), DROPEFFECT_COPY);
        let original_ticket = lease.borrow().as_ref().unwrap().clone();
        assert_eq!(enter(&target, &outer, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(fired.get(), "A's actual Leave observer must reenter");
        assert!(!original_ticket.is_current());
        let replacement_ticket = lease.borrow().as_ref().unwrap().clone();
        assert!(replacement_ticket.is_current());
        assert_eq!(drop_at(&target, &outer, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(outer_reads.borrow().is_empty());
        assert!(original_reads.borrow().is_empty());
        assert!(captures.borrow().is_empty());
        assert!(lease
            .borrow()
            .as_ref()
            .unwrap()
            .same_request(&replacement_ticket));
        assert_eq!(errors.get(), 0);
        assert_eq!(
            drop_at(&target, &replacement, DROPEFFECT_COPY),
            DROPEFFECT_COPY
        );
        let accepted = captures.borrow_mut().pop().unwrap();
        assert!(accepted.ticket.same_request(&replacement_ticket));
        assert_eq!(
            accepted.formats.text.as_deref(),
            Some("observer replacement B")
        );
        assert_eq!(replacement_reads.borrow().len(), 1);
        assert!(lease.borrow().is_none());
        // Break the test observer's retained target before the binding drops.
        target_slot.borrow_mut().take();
    })
    .join()
    .unwrap();
}
