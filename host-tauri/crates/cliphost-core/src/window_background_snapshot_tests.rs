use super::*;
use std::{cell::RefCell, ffi::OsStr, io};

#[test]
fn primary_path_keeps_its_label_on_success_failure_and_empty_value() {
    let root = PathBuf::from("captured-root");
    for initial in ["primary.PNG", "missing.jpg", ""] {
        for succeeds in [true, false] {
            let snapshot = CapturedBackgroundSource::capture(
                Some(initial.to_string()),
                "global.webp".to_string(),
                root.clone(),
            )
            .read_with(|path| {
                assert_eq!(path.as_os_str(), OsStr::new(initial));
                if succeeds {
                    Ok(vec![1, 2, 3])
                } else {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                }
            });
            assert_eq!(snapshot.captured_path.as_deref(), Some(initial));
            assert_eq!(snapshot.captured_data_root, root);
            if succeeds {
                assert_eq!(snapshot.content.as_ref().unwrap().0, vec![1, 2, 3]);
            } else {
                assert_eq!(snapshot.content, None);
            }
        }
    }
}

#[test]
fn absent_primary_uses_global_without_trimming_or_rebasing_paths() {
    let root = PathBuf::from("unrelated/data-root");
    for (primary, global, expected) in [
        (None, "relative/global.JPG", "relative/global.JPG"),
        (None, " ", " "),
        (
            Some("  relative/../source.PNG".to_string()),
            "fallback.webp",
            "  relative/../source.PNG",
        ),
    ] {
        let snapshot = CapturedBackgroundSource::capture(primary, global.to_string(), root.clone())
            .read_with(|path| {
                assert_eq!(path.as_os_str(), OsStr::new(expected));
                Ok(vec![4])
            });
        assert_eq!(snapshot.captured_path.as_deref(), Some(expected));
        assert_eq!(snapshot.captured_data_root, root);
        assert_eq!(snapshot.content.as_ref().unwrap().0, vec![4]);
    }
}

#[test]
fn no_selected_path_preserves_the_root_without_calling_reader() {
    let root = PathBuf::from("empty-source-root");
    let snapshot = CapturedBackgroundSource::capture(None, String::new(), root.clone())
        .read_with(|_| panic!("an absent source must not be read"));
    assert_eq!(snapshot.captured_path, None);
    assert_eq!(snapshot.captured_data_root, root);
    assert_eq!(snapshot.content, None);
}

#[test]
fn content_type_comes_from_captured_path_not_the_bytes() {
    for (initial, expected_mime) in [
        ("image.PNG", "image/png"),
        ("image.JpG", "image/jpeg"),
        ("image.jpeg", "image/jpeg"),
        ("image.gif", "image/gif"),
        ("image.webp", "image/webp"),
        ("image.bmp", "image/bmp"),
        ("image.bin", "application/octet-stream"),
        ("image", "application/octet-stream"),
    ] {
        let snapshot = CapturedBackgroundSource::capture(
            Some(initial.to_string()),
            String::new(),
            PathBuf::from("mime-root"),
        )
        .read_with(|_| Ok(vec![9, 8]));
        assert_eq!(
            snapshot.content,
            Some((vec![9, 8], expected_mime.to_string()))
        );
    }
}

#[test]
fn queued_a_request_captured_as_b_keeps_b_label_when_config_returns_to_a() {
    let requested_path = "A.png";
    for succeeds in [true, false] {
        let current = RefCell::new(requested_path.to_string());
        *current.borrow_mut() = "B.jpg".to_string();
        let captured = CapturedBackgroundSource::capture(
            Some(current.borrow().clone()),
            "global.webp".to_string(),
            PathBuf::from("captured-root"),
        );
        let snapshot = captured.read_with(|path| {
            assert_eq!(path, Path::new("B.jpg"));
            *current.borrow_mut() = requested_path.to_string();
            if succeeds {
                Ok(vec![0xb])
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        });
        assert_eq!(*current.borrow(), requested_path);
        assert_eq!(snapshot.captured_path.as_deref(), Some("B.jpg"));
        assert_eq!(
            snapshot.content,
            succeeds.then(|| (vec![0xb], "image/jpeg".to_string()))
        );
    }
}

#[test]
fn data_root_changed_during_read_does_not_relabel_the_captured_scope() {
    let original = PathBuf::from("root-before-read");
    let current = RefCell::new(original.clone());
    let captured = CapturedBackgroundSource::capture(
        Some("direct.png".to_string()),
        String::new(),
        current.borrow().clone(),
    );
    let snapshot = captured.read_with(|path| {
        assert_eq!(path, Path::new("direct.png"));
        *current.borrow_mut() = PathBuf::from("root-after-read");
        Ok(vec![5])
    });
    assert_eq!(snapshot.captured_data_root, original);
    assert_ne!(snapshot.captured_data_root, *current.borrow());
    assert_eq!(snapshot.captured_path.as_deref(), Some("direct.png"));
    assert_eq!(snapshot.content, Some((vec![5], "image/png".to_string())));
}

#[test]
fn actual_read_uses_owned_tmp_file_and_missing_primary_keeps_its_label() {
    let current_dir = std::env::current_dir().unwrap();
    let repo = current_dir
        .ancestors()
        .find(|root| root.join("host-tauri/crates/cliphost-core").is_dir())
        .expect("tests must run within the clip repository");
    let fixtures = repo.join(".tmp/background-snapshot/fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let path = fixtures.join(format!("actual-read-{}.PnG", std::process::id()));
    let missing = fixtures.join(format!("actual-missing-{}.JpG", std::process::id()));
    let bytes = vec![0x89, b'P', b'N', b'G', 0, 1, 2];
    std::fs::write(&path, &bytes).unwrap();
    assert!(!missing.exists());
    let root = fixtures.join("captured-data-root");
    let path_label = path.to_str().unwrap().to_string();
    let snapshot =
        CapturedBackgroundSource::capture(Some(path_label.clone()), String::new(), root.clone())
            .read();
    assert_eq!(snapshot.captured_path, Some(path_label.clone()));
    assert_eq!(snapshot.captured_data_root, root);
    assert_eq!(snapshot.content, Some((bytes, "image/png".to_string())));

    let missing_label = missing.to_str().unwrap().to_string();
    let failed =
        CapturedBackgroundSource::capture(Some(missing_label.clone()), path_label, root.clone())
            .read();
    assert_eq!(failed.captured_path, Some(missing_label));
    assert_eq!(failed.captured_data_root, root);
    assert_eq!(failed.content, None);
}

#[test]
fn bounded_absent_source_does_not_read_or_mark_oversized() {
    for limit in [0, 3] {
        let root = PathBuf::from("bounded-empty-root");
        let bounded = CapturedBackgroundSource::capture(None, String::new(), root.clone())
            .read_bounded_with(limit, |_, _| panic!("an absent source must not be read"));
        assert_eq!(bounded.snapshot.captured_path, None);
        assert_eq!(bounded.snapshot.captured_data_root, root);
        assert_eq!(bounded.snapshot.content, None);
        assert!(!bounded.oversized);
    }
}

#[test]
fn bounded_stream_handles_zero_and_exact_limit_boundaries() {
    use std::io::Cursor;

    for (bytes, limit, oversized) in [
        (&b""[..], 0, false),
        (&b"a"[..], 0, true),
        (&b"ab"[..], 3, false),
        (&b"abc"[..], 3, false),
        (&b"abcd"[..], 3, true),
        (&b"abc"[..], usize::MAX, false),
    ] {
        let result = read_bounded_stream(&mut Cursor::new(bytes), limit).unwrap();
        let expected = if oversized {
            BoundedRead::Oversized
        } else {
            BoundedRead::Content(bytes.to_vec())
        };
        assert_eq!(result, expected, "bytes={bytes:?}, limit={limit}");
    }

    let bytes = vec![0x7b; 64 * 1024 + 1];
    for limit in [64 * 1024, 64 * 1024 + 1, 64 * 1024 + 2] {
        let result = read_bounded_stream(&mut Cursor::new(&bytes), limit).unwrap();
        let expected = if limit < bytes.len() {
            BoundedRead::Oversized
        } else {
            BoundedRead::Content(bytes.clone())
        };
        assert_eq!(result, expected, "multi-chunk limit={limit}");
    }
}

#[test]
fn bounded_results_keep_captured_identity_on_content_oversize_and_failure() {
    for outcome in [
        Ok(BoundedRead::Content(vec![7])),
        Ok(BoundedRead::Oversized),
        Err(io::Error::from(io::ErrorKind::PermissionDenied)),
    ] {
        let root = PathBuf::from("bounded-captured-root");
        let current = RefCell::new(("B.PNG".to_string(), root.clone()));
        let expected_oversized = matches!(&outcome, Ok(BoundedRead::Oversized));
        let expected_content = match &outcome {
            Ok(BoundedRead::Content(bytes)) => Some((bytes.clone(), "image/png".to_string())),
            _ => None,
        };
        let captured = CapturedBackgroundSource::capture(
            Some(current.borrow().0.clone()),
            "global.webp".to_string(),
            current.borrow().1.clone(),
        );
        let bounded = captured.read_bounded_with(3, |path, limit| {
            assert_eq!(path, Path::new("B.PNG"));
            assert_eq!(limit, 3);
            *current.borrow_mut() = ("A.jpg".to_string(), PathBuf::from("bounded-new-root"));
            outcome
        });
        assert_eq!(bounded.snapshot.captured_path.as_deref(), Some("B.PNG"));
        assert_eq!(bounded.snapshot.captured_data_root, root);
        assert_eq!(bounded.snapshot.content, expected_content);
        assert_eq!(bounded.oversized, expected_oversized);
    }
}

#[test]
fn bounded_stream_retries_interrupted_reads_and_propagates_partial_read_errors() {
    use std::io::{Cursor, Read};

    struct InterruptedReader {
        bytes: Cursor<Vec<u8>>,
        calls: usize,
        terminal_error: Option<io::ErrorKind>,
    }
    impl Read for InterruptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            // Interrupt both the content read and the limit-exhausted EOF peek.
            if matches!(self.calls, 1 | 3) {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let count = self.bytes.read(buffer)?;
            if count == 0 {
                if let Some(kind) = self.terminal_error {
                    return Err(io::Error::from(kind));
                }
            }
            Ok(count)
        }
    }

    for terminal_error in [None, Some(io::ErrorKind::PermissionDenied)] {
        let mut reader = InterruptedReader {
            bytes: Cursor::new(b"abc".to_vec()),
            calls: 0,
            terminal_error,
        };
        let result = read_bounded_stream(&mut reader, 3);
        match terminal_error {
            None => assert_eq!(result.unwrap(), BoundedRead::Content(b"abc".to_vec())),
            Some(kind) => assert_eq!(result.unwrap_err().kind(), kind),
        }
        assert_eq!(reader.calls, 4);
    }
}

#[test]
fn bounded_file_read_detects_growth_after_in_limit_metadata_without_a_race() {
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
    };

    struct GrowingFile {
        reader: File,
        appender: File,
        grew: bool,
    }
    impl Read for GrowingFile {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.reader.read(buffer)?;
            if count > 0 && !self.grew {
                // Append after a real File read, so the next read sees growth
                // that was absent from the original handle's metadata.
                self.appender.write_all(b"cdef")?;
                self.appender.flush()?;
                self.grew = true;
            }
            Ok(count)
        }
    }

    let current_dir = std::env::current_dir().unwrap();
    let repo = current_dir
        .ancestors()
        .find(|root| root.join("host-tauri/crates/cliphost-core").is_dir())
        .expect("tests must run within the clip repository");
    let fixtures = repo.join(".tmp/background-snapshot/fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    let path = fixtures.join(format!("bounded-growth-{}.PNG", std::process::id()));
    let missing = fixtures.join(format!("bounded-missing-{}.jpg", std::process::id()));
    std::fs::write(&path, b"ab").unwrap();
    assert!(!missing.exists());
    let root = fixtures.join("bounded-captured-root");
    let label = path.to_str().unwrap().to_string();

    for limit in [0, 1, 2, 3] {
        let bounded =
            CapturedBackgroundSource::capture(Some(label.clone()), String::new(), root.clone())
                .read_bounded(limit);
        assert_eq!(
            bounded.snapshot.captured_path.as_deref(),
            Some(label.as_str())
        );
        assert_eq!(bounded.snapshot.captured_data_root, root);
        assert_eq!(bounded.oversized, limit < 2);
        assert_eq!(
            bounded.snapshot.content,
            (limit >= 2).then(|| (b"ab".to_vec(), "image/png".to_string()))
        );
    }

    let grown = CapturedBackgroundSource::capture(Some(label.clone()), String::new(), root.clone())
        .read_bounded_with(3, |path, limit| {
            let reader = File::open(path)?;
            assert_eq!(reader.metadata()?.len(), 2);
            assert!(reader.metadata()?.len() <= limit as u64);
            let mut growing = GrowingFile {
                reader,
                appender: OpenOptions::new().append(true).open(path)?,
                grew: false,
            };
            read_bounded_stream(&mut growing, limit)
        });
    assert!(grown.oversized);
    assert_eq!(grown.snapshot.content, None);
    assert_eq!(
        grown.snapshot.captured_path.as_deref(),
        Some(label.as_str())
    );
    assert_eq!(grown.snapshot.captured_data_root, root);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 6);

    let missing_label = missing.to_str().unwrap().to_string();
    let failed =
        CapturedBackgroundSource::capture(Some(missing_label.clone()), label, root.clone())
            .read_bounded(3);
    assert!(!failed.oversized);
    assert_eq!(failed.snapshot.content, None);
    assert_eq!(failed.snapshot.captured_path, Some(missing_label));
    assert_eq!(failed.snapshot.captured_data_root, root);
}
