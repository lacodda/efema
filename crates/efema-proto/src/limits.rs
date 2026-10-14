//! What a request may carry and a response may hold, in protocol version 1.
//!
//! Two kinds of number. *Ceilings* are the protocol's: no relay accepts more,
//! and a relay's operator can only go below them. *Defaults* are what a relay
//! enforces when its operator says nothing. A relay announces the limits it
//! enforces, per access key, as [`wire::Limits`](crate::wire::Limits); a
//! client cuts its batches and pages to what was announced, not to these.

use std::time::Duration;

/// The largest request body any relay accepts, in bytes - and the default. A
/// relay's operator may set a smaller one.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// The largest entry a relay accepts when its operator does not say, in
/// bytes: one page's worth of entry data, so a page that carries the largest
/// entry is no larger than any other.
pub const ENTRY_BYTES_DEFAULT: usize = PAGE_BYTES;
/// Bytes of entry data one space may hold, all its streams together, when the
/// operator does not say.
pub const SPACE_QUOTA_DEFAULT: u64 = 8 * 1024 * 1024 * 1024;
/// Requests one access key may make in a minute when the operator does not
/// say; as many may come at once.
pub const RATE_DEFAULT: u32 = 600;
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
