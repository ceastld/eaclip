//! Shared pure list-text recognition, also used by the history URL filter.

pub use cliphost_core::content_links::{
    collect_inline_links, utf16_hit_ranges, InlineLink, OpenTarget,
};
