//! What a request may carry and a response may hold, in protocol version 1.
//!
//! One definition for both sides: the relay enforces these, and a client cuts
//! its batches and pages to them. A relay that announces limits of its own
//! (planned) will announce them over these defaults; until then these are the
//! limits of every relay that speaks version 1.

use std::time::Duration;

/// The largest request body, in bytes.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Entries in a page when the reader does not say.
pub const PAGE_ENTRIES_DEFAULT: usize = 1000;
/// The most entries a reader may ask for in one page.
pub const PAGE_ENTRIES_MAX: usize = 10_000;
/// Bytes of entry data in one page, beyond which the page ends early. A page
/// always carries at least one entry, so no entry is unreadable.
pub const PAGE_BYTES: usize = 8 * 1024 * 1024;
/// How long a wait lasts when the waiter does not say.
pub const WAIT_DEFAULT: Duration = Duration::from_secs(30);
/// The longest wait. Below the idle timeouts of common reverse proxies, so a
/// wait ends with an answer rather than a dropped connection.
pub const WAIT_MAX: Duration = Duration::from_secs(60);
/// The most streams one wait may watch.
pub const WATCHES_MAX: usize = 64;
