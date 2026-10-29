use super::*;
use crate::native_content::model::TextProjection;
use gpui::ImageFormat;

const FOREGROUND: &str = "#222222";

fn projection(value: &str) -> ContentProjection {
    ContentProjection::Text(TextProjection {
        text: value.to_owned(),
        links: Vec::new(),
        hit_ranges: Vec::new(),
    })
}

fn cache(budget: &ContentBudget) -> ListContentCache {
    let mut cache = ListContentCache::with_budget(budget.clone());
    cache.advance(1);
    cache
}

fn resource(seed: usize) -> ContentResources {
    resource_with_dimensions(seed, 1, 1)
}

fn resource_with_dimensions(seed: usize, width: usize, height: usize) -> ContentResources {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(9_000_000);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let header = format!("<svg width=\"{width}\" height=\"{height}\"><!--budget-{seed}-{unique}");
    let mut bytes = Vec::with_capacity(300 * 1024);
    bytes.extend_from_slice(header.as_bytes());
    bytes.resize(300 * 1024 - 9, b'x');
    bytes.extend_from_slice(b"--></svg>");
    ContentResources {
        images: Default::default(),
        icons: [("icon".to_owned(), Some(Arc::new(Image::from_bytes(ImageFormat::Svg, bytes))))]
            .into_iter().collect(),
        paths: Default::default(),
    }
}

fn reserve(cache: &mut ListContentCache, id: &str) -> ContentLoadTicket {
    cache.reserve(1, id, &projection(id), ContentMode::Standard, FOREGROUND).unwrap()
}

fn insert(cache: &mut ListContentCache, id: &str, resources: ContentResources) -> bool {
    let ticket = reserve(cache, id);
    cache.finish(ticket, resources, true)
}

fn get<'a>(cache: &'a ListContentCache, id: &str) -> Option<&'a ContentResources> {
    cache.get(1, id, &projection(id), ContentMode::Standard, FOREGROUND)
}

// These fixtures never paint or instantiate App. Simulate completion of the
// CPU/atlas retirement barrier after inspecting its held accounting charge.
fn complete_unpainted_retirements(cache: &mut ListContentCache) -> Vec<Arc<Image>> {
    let images = cache.take_unshared_evicted();
    for image in &images {
        image_holders::acknowledge_image_retirement(image.id());
    }
    drop(std::mem::take(&mut cache.evicted_budget));
    images
}

#[test]
fn multiple_owners_obey_total_budget_and_retry_after_foreign_cleanup() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    let mut b = cache(&budget);
    assert!(insert(&mut a, "oldest", resource(1)));
    assert!(insert(&mut b, "second", resource(2)));
    assert!(insert(&mut a, "third", resource(3)));
    let charged = budget.snapshot().retained_bytes;
    assert!(!insert(&mut b, "fourth", resource(4)));
    assert_eq!(b.pending_len(), 0);
    assert!(get(&b, "fourth").is_none()); // never cache a permanent missing result
    assert_eq!(budget.snapshot().retained_bytes, charged);
    assert!(get(&a, "oldest").is_none()); // revoked resources cannot be painted
    assert!(get(&b, "second").is_some());
    assert!(get(&a, "third").is_some());
    assert!(a.apply_budget_pressure());
    assert_eq!(budget.snapshot().retained_bytes, charged); // still in release queue
    assert!(b.reserve(1, "fourth", &projection("fourth"), ContentMode::Standard, FOREGROUND).is_none());
    assert_eq!(complete_unpainted_retirements(&mut a).len(), 1);
    assert!(budget.snapshot().retained_bytes < charged);
    assert!(insert(&mut b, "fourth", resource(5)));
    assert!(get(&b, "fourth").is_some());
    assert!(budget.snapshot().retained_bytes <= budget.snapshot().limit_bytes);
    assert!(!budget.pressure_pending());
}

#[test]
fn pressure_on_the_same_owner_cannot_stall_permanently_or_revive_evicted_key() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    for id in ["first", "second", "third"] {
        assert!(insert(&mut a, id, resource(1)));
    }
    assert!(!insert(&mut a, "fourth", resource(2)));
    assert_eq!(a.len(), 2); // finish drained its own revocation
    assert!(get(&a, "first").is_none());
    assert!(!a.apply_budget_pressure());
    complete_unpainted_retirements(&mut a);
    assert!(insert(&mut a, "fourth", resource(3)));
    assert_eq!(a.len(), 3);
    assert!(get(&a, "first").is_none());
    assert!(get(&a, "fourth").is_some());
}

#[test]
fn stale_and_tampered_completions_do_not_charge_budget_or_leave_worker_slots() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    let stale = reserve(&mut a, "stale");
    a.advance(2);
    assert!(!a.finish(stale, resource(1), true));
    assert_eq!(a.pending_len(), 0);
    assert_eq!(budget.snapshot().entries, 0);
    a.advance(1);
    let mut tampered = reserve(&mut a, "tampered");
    tampered.key.foreground = "#ff0000".into();
    assert!(!a.finish(tampered, resource(2), true));
    assert_eq!(a.pending_len(), 0);
    assert_eq!(budget.snapshot().retained_bytes, 0);
    assert!(insert(&mut a, "current", resource(3)));
}

#[test]
fn invalidation_keeps_charge_until_retirement_and_preserves_a_shared_image_holder() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    let mut b = cache(&budget);
    let source = resource(1);
    let image = source.icons["icon"].as_ref().unwrap().clone();
    assert!(insert(&mut a, "a", source.clone()));
    assert!(insert(&mut b, "b", source));
    let used = budget.snapshot().retained_bytes;
    a.advance(2);
    assert_eq!(budget.snapshot().retained_bytes, used);
    assert!(image_holders::image_has_holders(image.id()));
    assert!(complete_unpainted_retirements(&mut a).is_empty());
    assert!(budget.snapshot().retained_bytes < used);
    assert!(image_holders::image_has_holders(image.id()));
    assert_eq!(get(&b, "b").unwrap().icons["icon"].as_ref().unwrap().id(), image.id());
    b.advance(2);
    assert!(!image_holders::image_has_holders(image.id()));
    assert_eq!(complete_unpainted_retirements(&mut b)[0].id(), image.id());
    assert_eq!(budget.snapshot().retained_bytes, 0);
}

#[test]
fn retaining_a_row_keeps_its_global_fifo_age_and_accounting_identity() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    let mut b = cache(&budget);
    assert!(insert(&mut a, "oldest", resource(1)));
    let before = budget.snapshot().retained_bytes;
    let image = get(&a, "oldest").unwrap().icons["icon"].as_ref().unwrap().clone();
    a.advance_retaining(2, |_| true);
    assert_eq!(budget.snapshot().retained_bytes, before);
    assert!(Arc::ptr_eq(a.get(2, "oldest", &projection("oldest"), ContentMode::Standard, FOREGROUND)
        .unwrap().icons["icon"].as_ref().unwrap(), &image));
    assert!(insert(&mut b, "second", resource(2)));
    assert!(insert(&mut b, "third", resource(3)));
    assert!(!insert(&mut b, "fourth", resource(4)));
    assert!(a.apply_budget_pressure());
    assert_eq!(a.len(), 0);
    assert_eq!(b.len(), 2);
    complete_unpainted_retirements(&mut a);
    assert!(insert(&mut b, "fourth", resource(5)));
}

#[test]
fn repeated_pressure_cycles_return_capacity_without_accumulating_retirement_reservations() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut a = cache(&budget);
    let mut b = cache(&budget);
    for round in 0..12 {
        for slot in 0..3 {
            let owner = if slot % 2 == 0 { &mut a } else { &mut b };
            assert!(insert(owner, &format!("{round}-{slot}"), resource(round * 3 + slot)));
        }
        assert!(!insert(&mut b, "rejected", resource(100 + round)));
        a.apply_budget_pressure();
        b.apply_budget_pressure();
        complete_unpainted_retirements(&mut a);
        complete_unpainted_retirements(&mut b);
        a.advance(round as u64 + 2);
        b.advance(round as u64 + 2);
        complete_unpainted_retirements(&mut a);
        complete_unpainted_retirements(&mut b);
        assert_eq!(budget.snapshot().retained_bytes, 0);
        assert_eq!(budget.snapshot().entries, 0);
        a.advance(1);
        b.advance(1);
    }
}

#[test]
fn stable_over_budget_working_sets_across_three_owners_stop_reloading() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut owners = [cache(&budget), cache(&budget), cache(&budget)];
    let ids = [["a0", "a1"], ["b0", "b1"], ["c0", "c1"]];
    for (owner, visible) in owners.iter_mut().zip(&ids) {
        owner.set_visible_items(visible.iter().copied());
    }
    let mut loads = 0;
    let mut after_settling = None;
    for round in 0..32 {
        for (owner, visible) in owners.iter_mut().zip(&ids) {
            // Owners may publish an identical viewport on every service tick.
            owner.set_visible_items(visible.iter().copied());
            for id in visible {
                if let Some(ticket) = owner.reserve(1, id, &projection(id), ContentMode::Standard, FOREGROUND) {
                    loads += 1;
                    owner.finish(ticket, resource(loads), true);
                }
            }
        }
        for owner in &mut owners {
            owner.apply_budget_pressure();
            complete_unpainted_retirements(owner);
        }
        assert!(budget.snapshot().retained_bytes <= budget.snapshot().limit_bytes);
        if round == 15 { after_settling = Some(loads); }
    }
    assert!(loads <= 18, "stable six-row working set kept reloading: {loads}");
    assert_eq!(loads, after_settling.unwrap(), "loads must stop without scrolling or changing content");
    assert!(owners.iter().map(|owner| owner.suppressed.len()).sum::<usize>() > 0);
    assert!(!budget.pressure_pending());
}

#[test]
fn local_byte_pressure_on_a_stable_visible_set_also_stops_reloading() {
    let budget = ContentBudget::new(128 * 1024 * 1024);
    let mut owner = cache(&budget);
    let ids = ["a", "b", "c", "d"];
    owner.set_visible_items(ids);
    let mut loads = 0;
    for round in 0..16 {
        for id in ids {
            if let Some(ticket) = owner.reserve(1, id, &projection(id), ContentMode::Standard, FOREGROUND) {
                loads += 1;
                assert!(owner.finish(ticket, resource_with_dimensions(loads, 2048, 1024), true));
                complete_unpainted_retirements(&mut owner);
            }
        }
        assert!(owner.retained_bytes() <= MAX_RETAINED_BYTES);
        if round > 0 { assert_eq!(loads, 4); }
    }
    assert_eq!(owner.suppressed.len(), 3);
    assert!(get(&owner, "d").is_some());
}

fn suppress_oldest_visible(owner: &mut ListContentCache) {
    owner.set_visible_items(["oldest", "second", "third", "fourth"]);
    for id in ["oldest", "second", "third"] {
        assert!(insert(owner, id, resource(1)));
    }
    assert!(!insert(owner, "fourth", resource(2)));
    complete_unpainted_retirements(owner);
    assert!(owner.suppressed.contains_key(&item_digest("oldest")));
}

#[test]
fn viewport_hide_reopen_and_generation_changes_rearm_suppressed_resources() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut owner = cache(&budget);
    suppress_oldest_visible(&mut owner);
    owner.set_visible_items(["oldest", "second", "third", "fourth"]);
    // The retirement barrier has released enough real capacity: an unchanged
    // viewport must now recover without requiring the user to scroll first.
    let recovered = reserve(&mut owner, "oldest");
    assert!(!owner.suppressed.contains_key(&item_digest("oldest")));
    assert!(!owner.suppressed.is_empty()); // the other rejected row is still tracked
    owner.set_visible_items(std::iter::empty());
    assert!(owner.suppressed.is_empty());
    assert!(!owner.finish(recovered, resource(3), true));
    assert_eq!(owner.pending_len(), 0);
    assert!(owner.reserve(1, "oldest", &projection("oldest"), ContentMode::Standard, FOREGROUND).is_none());
    owner.set_visible_items(["oldest"]);
    let ticket = reserve(&mut owner, "oldest");
    assert!(owner.finish(ticket, resource(3), true));
    owner.suppressed.insert(item_digest("oldest"), SuppressedResource {
        digest: resource_digest("oldest", &projection("oldest"), ContentMode::Standard, FOREGROUND),
        bytes: 0,
    });
    owner.advance_retaining(2, |_| true);
    assert!(owner.suppressed.is_empty());
    owner.suppressed.insert(item_digest("oldest"), SuppressedResource { digest: [0; 32], bytes: 0 });
    owner.advance(3);
    assert!(owner.suppressed.is_empty());
    complete_unpainted_retirements(&mut owner);
}

#[test]
fn changed_source_projection_mode_or_theme_can_retry_in_the_same_viewport() {
    for change in 0..3 {
        let budget = ContentBudget::new(1024 * 1024);
        let mut owner = cache(&budget);
        suppress_oldest_visible(&mut owner);
        let value = if change == 0 { projection("new source version") } else { projection("oldest") };
        let mode = if change == 1 { ContentMode::Minimal } else { ContentMode::Standard };
        let foreground = if change == 2 { "#ff0000" } else { FOREGROUND };
        let ticket = owner.reserve(1, "oldest", &value, mode, foreground).unwrap();
        assert!(!owner.suppressed.contains_key(&item_digest("oldest")));
        assert!(owner.finish(ticket, resource(3), true));
        assert!(owner.get(1, "oldest", &value, mode, foreground).is_some());
    }
}

#[test]
fn a_hidden_or_scrolled_away_inflight_reply_frees_its_slot_without_caching_a_failure() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut owner = cache(&budget);
    owner.set_visible_items(["a"]);
    let ticket = reserve(&mut owner, "a");
    owner.set_visible_items(std::iter::empty());
    assert!(!owner.finish(ticket, resource(1), true));
    assert_eq!(owner.pending_len(), 0);
    assert_eq!(owner.len(), 0);
    assert_eq!(budget.snapshot().retained_bytes, 0);
    owner.set_visible_items(["a"]);
    assert!(insert(&mut owner, "a", resource(2)));
}

#[test]
fn visible_identity_tracking_is_bounded_and_never_admits_untracked_tail_rows() {
    let budget = ContentBudget::new(1024 * 1024);
    let mut owner = cache(&budget);
    let ids: Vec<_> = (0..MAX_ENTRIES + 32).map(|n| format!("row-{n}")).collect();
    owner.set_visible_items(ids.iter().map(String::as_str));
    assert_eq!(owner.visible_items.as_ref().unwrap().len(), MAX_ENTRIES);
    assert!(owner.reserve(1, &ids[MAX_ENTRIES], &projection("tail"), ContentMode::Standard, FOREGROUND).is_none());
    assert_eq!(owner.pending_len(), 0);
    owner.set_visible_items([ids[MAX_ENTRIES].as_str()]);
    let ticket = owner.reserve(1, &ids[MAX_ENTRIES], &projection("tail"), ContentMode::Standard, FOREGROUND).unwrap();
    assert!(owner.finish(ticket, resource(1), true));
}
