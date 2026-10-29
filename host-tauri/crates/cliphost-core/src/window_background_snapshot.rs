//! Capture the source identity before reading a window background. A queued
//! read must not be labelled with a renderer's earlier or later config path.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowBackgroundSnapshot {
    pub captured_path: Option<String>,
    pub captured_data_root: PathBuf,
    pub content: Option<(Vec<u8>, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedWindowBackgroundSnapshot {
    pub snapshot: WindowBackgroundSnapshot,
    /// The captured source exceeded the limit; its partial content is discarded.
    /// An absent or unreadable source instead has `oversized == false`.
    pub oversized: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum BoundedRead {
    Content(Vec<u8>),
    Oversized,
}

/// Construct under the host's single config lock; consume after releasing it.
pub(crate) struct CapturedBackgroundSource {
    path: Option<String>,
    data_root: PathBuf,
}

impl CapturedBackgroundSource {
    pub(crate) fn capture(primary: Option<String>, global: String, data_root: PathBuf) -> Self {
        Self {
            path: primary.or_else(|| (!global.is_empty()).then_some(global)),
            data_root,
        }
    }

    pub(crate) fn read(self) -> WindowBackgroundSnapshot {
        // Preserve the old helper's direct stored-path interpretation. The
        // captured data root tags this resource scope; it does not rebase paths.
        self.read_with(|path| std::fs::read(path))
    }

    pub(crate) fn read_bounded(self, max_bytes: usize) -> BoundedWindowBackgroundSnapshot {
        self.read_bounded_with(max_bytes, |path, limit| {
            let mut file = File::open(path)?;
            // This is only an early rejection. The bounded read also detects
            // growth after metadata was captured, using the same open file.
            if file.metadata()?.len() > u64::try_from(limit).unwrap_or(u64::MAX) {
                return Ok(BoundedRead::Oversized);
            }
            read_bounded_stream(&mut file, limit)
        })
    }

    fn read_bounded_with(
        self,
        max_bytes: usize,
        read: impl FnOnce(&Path, usize) -> io::Result<BoundedRead>,
    ) -> BoundedWindowBackgroundSnapshot {
        let mut content = None;
        let mut oversized = false;
        if let Some(path) = self.path.as_deref() {
            let path = Path::new(path);
            match read(path, max_bytes) {
                Ok(BoundedRead::Content(bytes)) => {
                    content = Some((
                        bytes,
                        crate::window_background::resolve_content_type(path).into(),
                    ));
                }
                Ok(BoundedRead::Oversized) => oversized = true,
                Err(_) => {}
            }
        }
        BoundedWindowBackgroundSnapshot {
            snapshot: WindowBackgroundSnapshot {
                captured_path: self.path,
                captured_data_root: self.data_root,
                content,
            },
            oversized,
        }
    }

    fn read_with(
        self,
        read: impl FnOnce(&Path) -> std::io::Result<Vec<u8>>,
    ) -> WindowBackgroundSnapshot {
        let content = self.path.as_deref().and_then(|path| {
            let path = Path::new(path);
            read(path).ok().map(|bytes| {
                (
                    bytes,
                    crate::window_background::resolve_content_type(path).into(),
                )
            })
        });
        WindowBackgroundSnapshot {
            captured_path: self.path,
            captured_data_root: self.data_root,
            content,
        }
    }
}

fn read_bounded_stream(read: &mut impl Read, max_bytes: usize) -> io::Result<BoundedRead> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let remaining = max_bytes - bytes.len();
        if remaining == 0 {
            // A separate byte avoids both max+1 overflow and retaining content
            // beyond the limit. In particular, a zero limit still probes EOF.
            let mut extra = [0u8; 1];
            return match read_retry_interrupted(read, &mut extra)? {
                0 => Ok(BoundedRead::Content(bytes)),
                _ => Ok(BoundedRead::Oversized),
            };
        }
        let read_limit = remaining.min(chunk.len());
        let count = read_retry_interrupted(read, &mut chunk[..read_limit])?;
        if count == 0 {
            return Ok(BoundedRead::Content(bytes));
        }
        let next_len = bytes
            .len()
            .checked_add(count)
            .filter(|len| *len <= max_bytes)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
        if next_len > bytes.capacity() {
            // Bound requested growth rather than letting Vec double past the
            // limit. The allocator may still round up the requested capacity.
            let capacity = bytes
                .capacity()
                .saturating_mul(2)
                .max(next_len)
                .min(max_bytes);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn read_retry_interrupted(read: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match read.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
#[path = "window_background_snapshot_tests.rs"]
mod tests;
