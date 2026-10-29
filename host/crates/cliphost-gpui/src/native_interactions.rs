//! Per-root-window OLE and hover adapters. COM callbacks never borrow GPUI.
//! Drop admission fixes its destination on the STA before background decoding.

use crate::{
    drag_drop::{DropEvent, DropPayloadDecoder, DropTicket},
    hover::{HoverOwner, Owner},
    service::NativeHost,
};
use cliphost_core::{host::NativeDropRequest, HostState};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{mpsc, Arc},
};

#[path = "native_drop_writes.rs"]
mod writes;
pub use writes::{
    flush as flush_native_drop_writes, set_exit_pending as set_native_drop_exit_pending,
};
pub(crate) use writes::accept as accept_content_write;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropDestination {
    pub label: &'static str,
    pub parent_id: Option<String>,
    pub insert_index: i32,
}

pub enum NativeInteractionEvent {
    ContentSaved { operation_id:String, result:Result<Value,String> },
    DropSaved {
        request: NativeDropRequest,
        result: Result<Value, String>,
    },
    DragOver {
        ticket: DropTicket,
        over: bool,
    },
    DragFinished {
        session: u64,
        result: Result<Value, String>,
    },
    Error(String),
}

struct CoreDecoder;
impl DropPayloadDecoder for CoreDecoder {
    fn rich_text(&self, name: &str, bytes: &[u8]) -> Result<String, String> {
        HostState::decode_native_drop_rich_text(name, bytes)
    }
    fn quicker_action(&self, bytes: &[u8]) -> Result<String, String> {
        HostState::decode_native_drop_quicker_action(bytes)
    }
    fn dib_to_png(&self, bytes: &[u8], v5: bool) -> Result<Vec<u8>, String> {
        HostState::decode_native_drop_dib(bytes, v5)
    }
}

/// OLE callbacks and accepted drop workers cannot borrow GPUI. Publish their
/// reply before waking the root so a late completion survives a hidden owner.
struct NotifyingSender<T> {
    sender: mpsc::Sender<T>,
    wakeup: Arc<tokio::sync::Notify>,
}

impl<T> Clone for NotifyingSender<T> {
    fn clone(&self) -> Self {
        Self { sender: self.sender.clone(), wakeup: self.wakeup.clone() }
    }
}

impl<T> NotifyingSender<T> {
    fn send(&self, event: T) -> Result<(), mpsc::SendError<T>> {
        self.sender.send(event)?;
        self.wakeup.notify_one();
        Ok(())
    }
}

#[cfg(windows)]
pub struct NativeInteractions {
    host: Arc<NativeHost>,
    hwnd: isize,
    session: u64,
    destination: Rc<RefCell<DropDestination>>,
    incoming: crate::drag_drop::DragDropWindow,
    outgoing: crate::drag_drop::DragOutDispatcher,
    hover: Option<Arc<HoverOwner>>,
    menu_open: Cell<bool>,
    events: mpsc::Receiver<NativeInteractionEvent>,
    drag_replies: Vec<(DropTicket, mpsc::Receiver<Result<Value, String>>)>,
    content_drag_tokens: Vec<(DropTicket, String)>,
    drop_errors: mpsc::Receiver<crate::drag_drop::DropError>,
    incoming_lease: Rc<RefCell<Option<(DropTicket, u64)>>>,
}

#[cfg(windows)]
impl NativeInteractions {
    pub fn attach(
        host: Arc<NativeHost>,
        hwnd: isize,
        session: u64,
        destination: DropDestination,
    ) -> Result<Self, String> {
        if cliphost_core::is_headless() {
            return Err("Native interactions require a desktop window.".into());
        }
        let runtime = host.runtime_handle()?;
        let state = host.state();
        let destination = Rc::new(RefCell::new(destination));
        let (sender, events) = mpsc::channel();
        let (error_sender, drop_errors) = mpsc::channel();
        let wakeup = state.chrome_command_notifier();
        let sender = NotifyingSender { sender, wakeup: wakeup.clone() };
        let error_sender = NotifyingSender { sender: error_sender, wakeup };
        let drop_destination = destination.clone();
        let drop_host = state.clone();
        let drop_sender = sender.clone();
        let on_drop = Rc::new(move |event: DropEvent| {
            if let Some(token)=event.formats.internal_drag_token.as_deref() {
                event.ticket.verify()?;
                let prepared=crate::favorite_drag::accept_native(token,event.ticket.hwnd(),event.screen_point,event.control)?;
                if prepared.lan_conversation.is_some() {
                    return Err("LAN drops must be accepted by their conversation window.".into());
                }
                let accepted=writes::accept()?;
                if !prepared.drag.claim() {
                    let result=Err("The internal drag was already accepted or cancelled.".into());
                    accepted.finish(&result); return result.map(|_|());
                }
                prepared.drag.mark_native_accepted();
                let sender=drop_sender.clone();
                let operation_id=token.to_string();
                runtime.spawn_blocking(move || {
                    let result=prepared.host.favorite_drag_apply(&prepared.request);
                    accepted.finish(&result);
                    let _=sender.send(NativeInteractionEvent::ContentSaved {operation_id,result});
                });
                return Ok(());
            }
            let target = drop_destination.borrow().clone();
            let guard = event.ticket.clone();
            let request = drop_host.accept_native_drop(
                target.label,
                event.ticket.hwnd(),
                event.ticket.session(),
                target.parent_id.as_deref(),
                target.insert_index,
                move || guard.verify(),
            )?;
            let host = drop_host.clone();
            let sender = drop_sender.clone();
            let accepted = writes::accept()?;
            runtime.spawn_blocking(move || {
                let result = event
                    .formats
                    .into_snapshot(&CoreDecoder)
                    .and_then(|snapshot| host.save_native_drop(snapshot, &request));
                accepted.finish(&result);
                let _ = sender.send(NativeInteractionEvent::DropSaved { request, result });
            });
            Ok(())
        });
        let over_sender = sender.clone();
        let over_error_sender = error_sender.clone();
        let over_host = state.clone();
        let over_destination = destination.clone();
        let incoming_lease = Rc::new(RefCell::new(None::<(DropTicket, u64)>));
        let over_lease = incoming_lease.clone();
        let on_over = Rc::new(move |event| {
            let (ticket, over) = match event {
                crate::drag_drop::DragOverEvent::Enter(ticket) => (ticket, true),
                crate::drag_drop::DragOverEvent::Leave(ticket) => (ticket, false),
            };
            if over {
                if ticket.is_current() {
                    if let Some((_, id)) = over_lease.borrow_mut().take() {
                        over_host.release_native_incoming_drag(id);
                    }
                    match over_host.acquire_native_incoming_drag(
                        over_destination.borrow().label,
                        ticket.hwnd(),
                    ) {
                        Ok(id) => *over_lease.borrow_mut() = Some((ticket.clone(), id)),
                        Err(error) => {
                            let _ = over_error_sender.send(crate::drag_drop::DropError {
                                ticket: ticket.clone(),
                                message: error,
                            });
                        }
                    }
                }
            } else if over_lease
                .borrow()
                .as_ref()
                .is_some_and(|(held, _)| held.same_request(&ticket))
            {
                if let Some((_, id)) = over_lease.borrow_mut().take() {
                    over_host.release_native_incoming_drag(id);
                }
            }
            let _ = over_sender.send(NativeInteractionEvent::DragOver { ticket, over });
        });
        let on_error = Rc::new(move |error| {
            let _ = error_sender.send(error);
        });
        let incoming =
            crate::drag_drop::DragDropWindow::attach_stamped(hwnd, on_drop, on_over, on_error)?;
        let outgoing = crate::drag_drop::DragOutDispatcher::attach(
            hwnd,
            Arc::new(|payload, ticket| {
                let token=payload.internal_drag_token.clone();
                if let Some(token)=token.as_deref() {crate::favorite_drag::verify_native(token)?;}
                cliphost_core::ole_drag::begin_ole_drag_guarded(
                    payload,
                    Arc::new(move || {
                        ticket.verify()?;
                        if let Some(token)=token.as_deref() {crate::favorite_drag::native_guard(token)?;}
                        Ok(())
                    }),
                )
            }),
        )?;
        // Only the clipboard root installs the compatibility HTTP/Bridge runner.
        // Other source windows get operation-scoped runners via bound_host.
        if matches!(
            destination.borrow().label,
            "clipboard" | "history" | "favorites"
        ) {
            outgoing.install(&state);
        }
        let mut services = Self {
            host,
            hwnd,
            session: 0,
            destination,
            incoming,
            outgoing,
            hover: None,
            menu_open: Cell::new(false),
            events,
            drag_replies: Vec::new(),
            content_drag_tokens: Vec::new(),
            drop_errors,
            incoming_lease,
        };
        let initial = services.destination.borrow().clone();
        services.sync_session(session, true, initial)?;
        Ok(services)
    }

    pub fn sync_session(
        &mut self,
        session: u64,
        accepting: bool,
        destination: DropDestination,
    ) -> Result<(), String> {
        let rebound = self.session != session || *self.destination.borrow() != destination;
        if rebound || !accepting {
            crate::favorite_drag::clear_target_hwnd(self.hwnd);
            if let Some((_, id)) = self.incoming_lease.borrow_mut().take() {
                self.host.state().release_native_incoming_drag(id);
            }
        }
        // Same numerical session with a different destination must still
        // retire tickets before accepting the new folder.
        let preserve_content_source=accepting && !self.content_drag_tokens.is_empty();
        if rebound && self.session == session {
            self.incoming.sync_session(session, false);
            if !preserve_content_source {self.outgoing.sync_session(session, false);}
        }
        self.incoming.sync_session(session, accepting);
        if !preserve_content_source {self.outgoing.sync_session(session, accepting);}
        *self.destination.borrow_mut() = destination;
        if !accepting {
            if let Some(owner) = &self.hover {
                owner.retire();
            }
            self.hover = None;
        } else if rebound || !self.hover.as_ref().is_some_and(|owner| owner.is_current()) {
            if let Some(owner) = &self.hover {
                owner.retire();
            }
            self.hover = None;
            self.hover = Some(Arc::new(self.host.hover_service()?.bind_owner(Owner {
                hwnd: self.hwnd,
                session,
            })?));
            self.hover
                .as_ref()
                .unwrap()
                .set_menu_open(self.menu_open.get())?;
        }
        self.session = session;
        Ok(())
    }

    pub fn hover_owner(&self) -> Option<Arc<HoverOwner>> {
        self.hover.clone()
    }
    pub fn cancel_hover(&self) {
        if let Some(owner) = &self.hover {
            owner.cancel();
        }
    }
    pub fn set_menu_open(&self, open: bool) -> Result<(), String> {
        self.menu_open.set(open);
        if let Some(owner) = &self.hover {
            owner.set_menu_open(open)?;
        }
        Ok(())
    }
    pub fn start_history(&mut self, ids: Vec<String>) -> Result<(), String> {
        self.cancel_hover();
        self.drag_replies.push(
            self.outgoing
                .start_history_ticketed(&self.host.state(), ids)?,
        );
        Ok(())
    }
    pub fn start_favorite(&mut self, id: String) -> Result<(), String> {
        self.start_favorites(vec![id])
    }
    pub fn start_favorites(&mut self, ids: Vec<String>) -> Result<(), String> {
        self.cancel_hover();
        let host = self.outgoing.bound_host(&self.host.state());
        self.drag_replies.push(
            self.outgoing
                .start_ticketed(move || host.begin_favor_ole_drag(&json!({"ids":ids})))?,
        );
        Ok(())
    }
    pub(crate) fn start_content_drag(&mut self, drag: crate::favorite_drag::ContentDrag) -> Result<(), String> {
        self.cancel_hover();
        crate::favorite_drag::begin_native(drag.clone())?;
        let token=drag.operation_id.clone();
        let host=self.outgoing.bound_host(&self.host.state());
        let kind=match drag.source {crate::favorite_drag::DragSource::History{..}=>"history",_=>"favorite"};
        let request=json!({"ids":drag.item_ids,"sourceKind":kind,"internalDragToken":token});
        match self.outgoing.start_ticketed(move || host.begin_favor_ole_drag(&request)) {
            Ok(reply)=> {self.content_drag_tokens.push((reply.0.clone(),token)); self.drag_replies.push(reply);Ok(())},
            Err(error)=> {crate::favorite_drag::finish_native(&token);Err(error)}
        }
    }
    pub fn drain(&mut self) -> Vec<NativeInteractionEvent> {
        let mut events = self.events.try_iter().collect::<Vec<_>>();
        let content_tokens=&mut self.content_drag_tokens;
        self.drag_replies
            .retain(|(ticket, reply)| match reply.try_recv() {
                Ok(result) => {
                    if let Some(index)=content_tokens.iter().position(|(current,_)| current.same_request(ticket)) {
                        let (_,token)=content_tokens.remove(index); crate::favorite_drag::finish_native(&token);
                    }
                    if ticket.is_current() {
                        events.push(NativeInteractionEvent::DragFinished {
                            session: ticket.session(),
                            result,
                        });
                    }
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(index)=content_tokens.iter().position(|(current,_)| current.same_request(ticket)) {
                        let (_,token)=content_tokens.remove(index); crate::favorite_drag::finish_native(&token);
                    }
                    if ticket.is_current() {
                        events.push(NativeInteractionEvent::DragFinished {
                            session: ticket.session(),
                            result: Err("Native drag worker was interrupted.".into()),
                        });
                    }
                    false
                }
            });
        for error in self
            .drop_errors
            .try_iter()
            .filter(|error| error.ticket.is_current())
        {
            events.push(NativeInteractionEvent::Error(error.message));
        }
        if let Some(error) = self.hover.as_ref().and_then(|owner| owner.take_error()) {
            // Passive preview failures already retire and hide their popup.
            // Do not replace action feedback with a persistent list error.
            tracing::debug!(message = %error.message, "Hover preview unavailable");
        }
        events
    }
}

#[cfg(windows)]
impl Drop for NativeInteractions {
    fn drop(&mut self) {
        for (_,token) in self.content_drag_tokens.drain(..) {crate::favorite_drag::finish_native(&token);}
        self.cancel_hover();
        if let Some(owner) = &self.hover {
            owner.retire();
        }
        self.incoming.cancel();
        if let Some((_, id)) = self.incoming_lease.borrow_mut().take() {
            self.host.state().release_native_incoming_drag(id);
        }
    }
}
