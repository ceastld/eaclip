//! Wake every list cache on shared-budget pressure, including hidden views.
//! Weak owners avoid keeping a window or its cached pictures alive.

use std::{collections::HashMap, sync::Arc};

use gpui::{App, Context, Global, Task};

use crate::native_content::budget;

type Owner = Box<dyn FnMut(&mut App) -> bool>;

struct CacheOwners {
    owners: HashMap<u64, Owner>,
    _task: Task<()>,
}

impl Global for CacheOwners {}

pub(crate) fn register<T: 'static>(cx: &mut Context<T>, drain: fn(&mut T, &mut Context<T>)) {
    if !cx.has_global::<CacheOwners>() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
        let task = cx.spawn(async move |_, cx| {
            while receiver.recv().await.is_some() {
                if cx
                    .update(|cx| {
                        budget::acknowledge_shared_content_budget_pressure();
                        // Updating another entity can register more owners. Do not
                        // hold the global borrow across those entity updates.
                        let mut owners = std::mem::take(&mut cx.global_mut::<CacheOwners>().owners);
                        owners.retain(|_, drain| drain(cx));
                        cx.global_mut::<CacheOwners>().owners.extend(owners);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        cx.set_global(CacheOwners {
            owners: HashMap::new(),
            _task: task,
        });
        budget::set_shared_content_budget_pressure_notifier(Arc::new(move || {
            let _ = sender.send(());
        }));
    }
    let id = cx.entity_id().as_u64();
    if cx.global::<CacheOwners>().owners.contains_key(&id) {
        return;
    }
    let owner = cx.weak_entity();
    cx.global_mut::<CacheOwners>()
        .owners
        .insert(id, Box::new(move |cx| owner.update(cx, drain).is_ok()));
    cx.on_release(move |_, cx| {
        if cx.has_global::<CacheOwners>() {
            cx.global_mut::<CacheOwners>().owners.remove(&id);
        }
    })
    .detach();
}
