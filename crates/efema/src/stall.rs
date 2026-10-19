//! Telling a connection that went silent from one that is slow.
//!
//! A phone that drops off a network mid-request, or a proxy whose upstream
//! hung, leaves a connection open and silent: no error and no bytes, until
//! TCP itself gives up minutes later. A time limit on the whole request
//! cannot tell that from a large batch going up a slow network - one short
//! enough to catch the silence cuts the slow upload every time. So a request
//! is held to progress instead: it may take as long as it keeps moving, and
//! is given up on once nothing has moved for [`STALL`].
//!
//! What moves is the request's body being taken by the connection, the
//! answer's headers arriving, and each piece of the answer's body. Between
//! the last of the request and the first of the answer the relay does its
//! work - [`STALL`] for that, or longer for a request that asks the relay to
//! hold its answer, as a wait does. And the connection takes a body ahead of
//! the network, into its buffers and the socket's: when the last piece is
//! taken, up to [`AHEAD`] of it may still be on its way up, so the answer is
//! also allowed the time that takes on the slowest network waited for.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::time::Instant;

/// How long a request to the relay may go without moving a byte - none of it
/// taken by the connection, none of the answer arriving - before the client
/// gives up on the connection as stalled. A request that keeps moving is
/// never cut, however long it takes.
pub const STALL: Duration = Duration::from_secs(30);

/// How much of a request's body the connection is handed at a time: each
/// piece it takes is progress.
const PIECE: usize = 16 * 1024;

/// How much of a body the connection may hold taken but not yet sent when
/// its last piece is taken: the HTTP client's buffer - some 400 KiB - and the
/// socket's, with room to spare.
const AHEAD: usize = 1024 * 1024;

/// The slowest network a request is waited for on, in bytes a second: about
/// 64 kbit/s.
const SLOWEST: usize = 8 * 1024;

/// How long the end of a `len`-byte body may still take to go up once its
/// last piece is taken, on the slowest network.
fn on_its_way(len: usize) -> Duration {
    Duration::from_millis((len.min(AHEAD) * 1000 / SLOWEST) as u64)
}

/// When a request last moved, and how long it may stay still from there.
#[derive(Clone)]
pub(crate) struct Pulse(Arc<Mutex<Beat>>);

struct Beat {
    at: Instant,
    allowed: Duration,
}

impl Pulse {
    /// A pulse that starts now, allowed `allowed` until its first beat.
    pub(crate) fn new(allowed: Duration) -> Self {
        Self(Arc::new(Mutex::new(Beat { at: Instant::now(), allowed })))
    }

    /// Something moved; the next move may take `allowed`.
    pub(crate) fn beat(&self, allowed: Duration) {
        *self.lock() = Beat { at: Instant::now(), allowed };
    }

    /// Resolves once nothing has moved for as long as was allowed, with that
    /// allowance - and never while things keep moving.
    pub(crate) async fn silence(&self) -> Duration {
        loop {
            let deadline = {
                let beat = self.lock();
                beat.at + beat.allowed
            };
            tokio::time::sleep_until(deadline).await;
            let beat = self.lock();
            if Instant::now() >= beat.at + beat.allowed {
                return beat.allowed;
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Beat> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A request body handed to the connection a piece at a time, each piece it
/// takes a beat of the pulse. The last one hands the request over: from then
/// the answer has `answer_within` to begin, and the end of the body the time
/// it may take to go up.
pub(crate) struct Upload {
    rest: Bytes,
    pulse: Pulse,
    handed_over: Duration,
}

impl Upload {
    pub(crate) fn new(body: Vec<u8>, pulse: Pulse, answer_within: Duration) -> Self {
        let handed_over = answer_within + on_its_way(body.len());
        Self { rest: Bytes::from(body), pulse, handed_over }
    }
}

impl Body for Upload {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.rest.is_empty() {
            return Poll::Ready(None);
        }
        let len = self.rest.len().min(PIECE);
        let piece = self.rest.split_to(len);
        let allowed = if self.rest.is_empty() { self.handed_over } else { STALL };
        self.pulse.beat(allowed);
        Poll::Ready(Some(Ok(Frame::data(piece))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.rest.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The next piece of `upload`, as the connection takes it.
    async fn take(upload: &mut Upload) -> Option<usize> {
        let frame = std::future::poll_fn(|cx| Pin::new(&mut *upload).poll_frame(cx)).await?;
        Some(frame.unwrap().into_data().unwrap().len())
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_keeps_moving_is_never_taken_for_stalled() {
        let started = Instant::now();
        let pulse = Pulse::new(STALL);
        let beating = pulse.clone();
        // Moving every 20 seconds for 200 seconds - far longer than STALL as
        // a whole, never that long between moves.
        tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_secs(20)).await;
                beating.beat(STALL);
            }
        });
        assert_eq!(pulse.silence().await, STALL);
        assert_eq!(started.elapsed(), Duration::from_secs(200) + STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn an_upload_that_stops_being_taken_is_stalled() {
        let started = Instant::now();
        let pulse = Pulse::new(STALL);
        let mut upload = Upload::new(vec![7; 3 * PIECE], pulse.clone(), Duration::from_secs(600));
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert_eq!(take(&mut upload).await, Some(PIECE));
        // Two pieces are left, and the connection takes neither.
        assert_eq!(pulse.silence().await, STALL);
        assert_eq!(started.elapsed(), Duration::from_secs(25) + STALL);
    }

    #[tokio::test(start_paused = true)]
    async fn the_answer_may_take_what_the_request_allows_once_it_is_handed_over() {
        let pulse = Pulse::new(STALL);
        let answer_within = Duration::from_secs(90);
        // 32 KiB and a byte: four seconds and a little on its way, at 8 KiB a
        // second.
        let len = 2 * PIECE + 1;
        let mut upload = Upload::new(vec![7; len], pulse.clone(), answer_within);
        assert_eq!(upload.size_hint().exact(), Some(len as u64));
        let mut taken = Vec::new();
        while let Some(len) = take(&mut upload).await {
            taken.push(len);
        }
        assert_eq!(taken, [PIECE, PIECE, 1]);
        assert!(upload.is_end_stream());
        assert_eq!(upload.size_hint().exact(), Some(0));

        let handed_over = Instant::now();
        let allowed = answer_within + Duration::from_millis(4_000);
        assert_eq!(pulse.silence().await, allowed);
        assert_eq!(handed_over.elapsed(), allowed);
    }

    #[test]
    fn what_may_still_be_on_its_way_is_bounded_by_the_buffers() {
        assert_eq!(on_its_way(0), Duration::ZERO);
        assert_eq!(on_its_way(SLOWEST), Duration::from_secs(1));
        assert_eq!(on_its_way(AHEAD), Duration::from_secs(128));
        assert_eq!(on_its_way(16 * AHEAD), Duration::from_secs(128));
    }
}
