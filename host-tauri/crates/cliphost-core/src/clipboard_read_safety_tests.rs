use super::*;

#[test]
fn clipboard_read_safety_text_result_never_requests_later_formats() {
    let mut calls = Vec::new();
    let item = capture_in_order("fixture.exe", false, |bucket| {
        calls.push(bucket);
        assert_eq!(bucket, '0', "a successful ordinary text capture requested unrelated formats");
        Ok::<_, ()>(try_capture_bucket(&ClipboardSnapshot { text: Some("owned text".into()), ..Default::default() },
            Path::new(""), "fixture.exe", true, "", bucket, &|_, _| None))
    }).unwrap().unwrap();
    assert_eq!(item.body_text.as_deref(), Some("owned text"));
    assert_eq!(calls, vec!['0']);
}

#[test]
fn clipboard_read_safety_im_and_browser_keep_image_file_priority() {
    for source in ["wxwork.exe", "wechat.exe", "qq.exe", "chrome.exe"] {
        let mut calls = Vec::new();
        let selected = capture_in_order(source, true, |bucket| {
            calls.push(bucket);
            Ok::<_, ()>((bucket == '4').then_some("image"))
        }).unwrap();
        assert_eq!(selected, Some("image"));
        assert_eq!(calls, vec!['4']);
    }
    let mut calls = Vec::new();
    let selected = capture_in_order("chrome.exe", true, |bucket| {
        calls.push(bucket);
        Ok::<_, ()>((bucket == '3').then_some("files"))
    }).unwrap();
    assert_eq!(selected, Some("files"));
    assert_eq!(calls, vec!['4', '3']);
}

#[test]
fn clipboard_read_safety_disabled_steps_preserve_action_capture() {
    for source in ["fixture.exe", "wxwork.exe", "chrome.exe"] {
        let mut calls = Vec::new();
        assert_eq!(capture_in_order::<(), ()>(source, false, |bucket| { calls.push(bucket); Ok(None) }).unwrap(), None);
        assert!(calls.contains(&'1') && !calls.contains(&'2'));
        assert_eq!(calls.len(), 4);
    }
    let mut calls = Vec::new();
    capture_in_order::<(), ()>("fixture.exe", true, |bucket| { calls.push(bucket); Ok(None) }).unwrap();
    assert_eq!(calls, vec!['0', '1', '2', '3', '4']);
    let mut calls = Vec::new();
    let item = capture_in_order("fixture.exe", false, |bucket| {
        calls.push(bucket);
        Ok::<_, ()>(try_capture_bucket(&ClipboardSnapshot {
            quicker_action: Some("owned-action-id".into()), ..Default::default()
        }, Path::new(""), "fixture.exe", true, "", bucket, &|_, _| None))
    }).unwrap().unwrap();
    assert_eq!(item.kind, ClipKind::ActionClipItem);
    assert_eq!(item.body_text.as_deref(), Some("owned-action-id"));
    assert_eq!(calls, vec!['0', '1']);
    assert!(monitored_kind_allowed(item.kind, false), "The actual monitor filter discarded an enabled Action capture");
    assert!(!monitored_kind_allowed(ClipKind::StepClipItem, false));
    assert!(monitored_kind_allowed(ClipKind::StepClipItem, true));
}

#[test]
fn clipboard_read_safety_rejected_text_falls_back_to_files() {
    let mut calls = Vec::new();
    let item = capture_in_order("fixture.exe", false, |bucket| {
        calls.push(bucket);
        let snapshot = ClipboardSnapshot { text: Some("   ".into()), files: vec!["owned-fixture.txt".into()], ..Default::default() };
        Ok::<_, ()>(try_capture_bucket(&snapshot, Path::new(""), "fixture.exe", true, "", bucket, &|_, _| None))
    }).unwrap().unwrap();
    assert_eq!(item.kind, ClipKind::FileClipItem);
    assert_eq!(calls, vec!['0', '1', '3']);
}

#[test]
fn clipboard_read_safety_changed_copy_aborts_before_reading_later_buckets() {
    let mut calls = Vec::new();
    let result = capture_in_order::<(), _>("fixture.exe", true, |bucket| {
        calls.push(bucket); Err(ClipboardReadFailure::Changed)
    });
    assert!(matches!(result, Err(ClipboardReadFailure::Changed)));
    assert_eq!(calls, vec!['0']);
}

#[test]
fn clipboard_read_safety_text_candidates_continue_after_real_rejection() {
    let snapshot = ClipboardSnapshot { wechat_rich: Some(" ".into()),
        qq_rich: Some("must not replace the present WeChat candidate".into()),
        html: Some("<p>owned text</p>".into()), text: Some("owned text".into()), ..Default::default() };
    let transforms = std::cell::Cell::new(0);
    let mut requests = Vec::new();
    let selected = capture_text_in_order::<()>(|candidate| {
        requests.push(candidate);
        Ok(try_capture_text_candidate(&snapshot, Path::new(""), "fixture.exe", true, "", candidate,
            &|_, _| {
                let count = transforms.get();
                transforms.set(count + 1);
                Some(if count == 0 { "".into() } else { "accepted text".into() })
            }))
    }).unwrap().unwrap();
    assert_eq!(requests, vec![TextCaptureCandidate::Rich, TextCaptureCandidate::Html, TextCaptureCandidate::Text]);
    assert_eq!(transforms.get(), 2, "A rejected rich candidate was preprocessed or HTML was retried");
    assert_eq!(selected.kind, ClipKind::TextClipItem);
    assert_eq!(selected.body_text.as_deref(), Some("accepted text"));
}

#[test]
fn clipboard_read_safety_accepted_rich_never_requests_html_or_plain_text() {
    let snapshot = ClipboardSnapshot { wechat_rich: Some("owned rich text".into()),
        text: Some("owned fallback".into()), ..Default::default() };
    let mut requests = Vec::new();
    let selected = capture_text_in_order::<()>(|candidate| {
        requests.push(candidate);
        Ok(try_capture_text_candidate(&snapshot, Path::new(""), "fixture.exe", true, "", candidate, &|_, _| None))
    }).unwrap().unwrap();
    assert_eq!(requests, vec![TextCaptureCandidate::Rich]);
    assert_eq!(selected.kind, ClipKind::QQRichTextItem);
}
