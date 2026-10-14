//! One report on a device's sync, the same in every app.
//!
//! An app shows it under its own command - `scheda doctor`, `kilna doctor` -
//! and none of them writes its own: what the relay says, what the access key
//! may do and how full its space is, how far behind the device is, what waits
//! on it to be sent, how old its key is.

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use efema_proto::wire::Limits;
use efema_proto::{Cursor, Epoch, StreamId, StreamName};
use lacodda_seal::{KdfParams, KeyId};

use crate::client::Client;
use crate::state::Queue;
use crate::transport::Transport;
use crate::{DeviceId, Error};

/// How long items may wait on a device before the wait is a problem: a day
/// without the relay taking them is more than a network that comes and goes.
pub const QUEUE_STALE: Duration = Duration::from_secs(24 * 60 * 60);

/// How full a space may get before it is a problem: past nine tenths, the
/// next large push may be the one the relay refuses.
pub const SPACE_FULL: f64 = 0.9;

/// A device's sync, as [`Client::doctor`] finds it.
#[derive(Debug)]
pub struct Report {
    /// Where the transport leads.
    pub transport: String,
    /// The stream.
    pub stream: StreamName,
    /// The incarnation this device syncs with.
    pub stream_id: StreamId,
    /// This device.
    pub device: DeviceId,
    /// The epoch the app writes in and reads up to.
    pub epoch: Epoch,
    /// The device's acknowledged cursor, or why it could not be read.
    pub cursor: Result<Cursor, String>,
    /// What waits on the device to be sent, or why it could not be read.
    pub queue: Result<Queue, String>,
    /// The stream's key.
    pub key: KeyReport,
    /// What the relay said, or why it said nothing.
    pub relay: Result<RelayReport, String>,
    /// What the relay holds the access key to - whether it writes, how full
    /// its space is - or why that could not be learnt.
    pub access: Result<Limits, String>,
}

/// The stream's key, as far as the device can tell without the passphrase.
#[derive(Debug)]
pub struct KeyReport {
    /// Its identity - the same on every device that syncs the stream.
    pub id: KeyId,
    /// How the passphrase is stretched to unlock it.
    pub params: KdfParams,
    /// When it was locked under the passphrase it has now.
    pub locked_at: SystemTime,
    /// How many times the passphrase has been changed.
    pub changes: u32,
}

/// The stream as the relay has it.
#[derive(Debug)]
pub struct RelayReport {
    /// How long the relay took to answer.
    pub latency: Duration,
    /// The stream's last position.
    pub head: u64,
    /// The stream's epoch.
    pub epoch: Epoch,
}

impl Report {
    /// How many entries the device has yet to read, when both the cursor and
    /// the relay are known.
    pub fn behind(&self) -> Option<u64> {
        match (&self.cursor, &self.relay) {
            (Ok(cursor), Ok(relay)) => Some(relay.head.saturating_sub(cursor.seq)),
            _ => None,
        }
    }

    /// What is wrong, one line each; empty when nothing is.
    pub fn problems(&self) -> Vec<String> {
        self.problems_at(SystemTime::now())
    }

    fn problems_at(&self, now: SystemTime) -> Vec<String> {
        let mut problems = Vec::new();
        if let Err(e) = &self.cursor {
            problems.push(format!("the cursor cannot be read: {e}"));
        }
        match &self.queue {
            Err(e) => problems.push(format!("the queue cannot be read: {e}")),
            Ok(Queue { items, oldest: Some(oldest), .. })
                if now.duration_since(*oldest).is_ok_and(|waited| waited > QUEUE_STALE) =>
            {
                problems.push(format!(
                    "{items} {} waited to be sent since {}: send again, and if the relay refuses, the error says why",
                    plural(*items as u64, "item has", "items have"),
                    when(*oldest, now)
                ));
            }
            Ok(_) => {}
        }
        match (&self.access, &self.queue) {
            (Ok(access), Ok(queue)) if !access.write && !queue.is_empty() => problems.push(format!(
                "{} {} to be sent, and the access key only reads: the relay will not take {}",
                queue.items,
                plural(queue.items as u64, "item waits", "items wait"),
                if queue.items == 1 { "it" } else { "them" }
            )),
            _ => {}
        }
        if let Ok(access) = &self.access
            && access.quota > 0
            && access.used as f64 >= access.quota as f64 * SPACE_FULL
        {
            problems.push(format!(
                "the space holds {} of the {} it may: once it is full, the relay takes nothing more until its operator gives it room",
                size(access.used),
                size(access.quota)
            ));
        }
        match &self.relay {
            Err(e) => problems.push(e.clone()),
            Ok(relay) if relay.epoch > self.epoch => problems.push(format!(
                "the stream is at epoch {}, newer than this app's {}: update the app to read and write it",
                relay.epoch, self.epoch
            )),
            Ok(_) => {}
        }
        if self.key.params.memory_kib() < KdfParams::DEFAULT.memory_kib() {
            problems.push(format!(
                "the key is locked with {}, weaker than today's default {}: changing the passphrase locks it again",
                self.key.params,
                KdfParams::DEFAULT
            ));
        }
        problems
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let relay = match &self.relay {
            Ok(relay) => format!("{} - answered in {} ms", self.transport, relay.latency.as_millis()),
            Err(_) => format!("{} - no answer", self.transport),
        };
        let stream = match &self.relay {
            Ok(relay) => format!(
                "{} - stream {}, epoch {}, {} {}",
                self.stream,
                short(&self.stream_id.to_string()),
                relay.epoch,
                relay.head,
                plural(relay.head, "entry", "entries")
            ),
            Err(_) => format!("{} - stream {}", self.stream, short(&self.stream_id.to_string())),
        };
        let cursor = match (&self.cursor, self.behind()) {
            (Ok(cursor), Some(0)) => format!("at {} - up to date", cursor.seq),
            (Ok(cursor), Some(behind)) => {
                format!("at {} - {behind} {} to read", cursor.seq, plural(behind, "entry", "entries"))
            }
            (Ok(cursor), None) => format!("at {}", cursor.seq),
            (Err(_), _) => "unknown".to_string(),
        };
        let queue = match &self.queue {
            Ok(queue) if queue.is_empty() => "empty".to_string(),
            Ok(queue) => format!(
                "{} {} ({}) to send, the oldest since {}",
                queue.items,
                plural(queue.items as u64, "item", "items"),
                size(queue.bytes),
                queue.oldest.map_or_else(|| "-".to_string(), |oldest| when(oldest, SystemTime::now()))
            ),
            Err(_) => "unknown".to_string(),
        };
        let access = match &self.access {
            Ok(access) => format!(
                "{} - {} of {} in the space",
                if access.write { "reads and writes" } else { "reads only" },
                size(access.used),
                size(access.quota)
            ),
            Err(_) => "unknown".to_string(),
        };
        let mut key =
            format!("{} - locked {}, {}", self.key.id, when(self.key.locked_at, SystemTime::now()), self.key.params);
        match self.key.changes {
            0 => {}
            1 => key.push_str(", passphrase changed once"),
            n => key.push_str(&format!(", passphrase changed {n} times")),
        }

        writeln!(f, "relay    {relay}")?;
        writeln!(f, "access   {access}")?;
        writeln!(f, "stream   {stream}")?;
        writeln!(f, "cursor   {cursor}")?;
        writeln!(f, "queue    {queue}")?;
        writeln!(f, "key      {key}")?;
        writeln!(f, "device   {}", self.device.short())?;
        write!(f, "epoch    {} - written, and read up to", self.epoch)?;
        for problem in self.problems() {
            write!(f, "\n!        {problem}")?;
        }
        Ok(())
    }
}

impl<T: Transport> Client<T> {
    /// Looks at this device's sync: asks the relay for the stream from the
    /// device's cursor and for its limits - keeping them, as a send would -
    /// and reports what it found next to what the device knows. Never fails -
    /// what cannot be found out is in the report.
    pub async fn doctor(&self) -> Report {
        let cursor = self.cursor().await.map_err(|e| e.to_string());
        let queue = self.queue().await.map_err(|e| e.to_string());
        let started = Instant::now();
        let relay = match &cursor {
            Ok(cursor) => match self.transport().read(self.stream(), Some(cursor), 1).await {
                Ok(page) => Ok(RelayReport { latency: started.elapsed(), head: page.head, epoch: page.epoch }),
                Err(e) => Err(diagnose(self.stream(), cursor, e)),
            },
            Err(e) => Err(e.clone()),
        };
        let access = self.refresh_limits().await.map_err(|e| e.to_string());
        let lock = self.locked_key();
        let changes = self.passphrase_changes();
        Report {
            transport: self.transport().describe(),
            stream: self.stream().clone(),
            stream_id: self.stream_id(),
            device: self.device(),
            epoch: self.epoch(),
            cursor,
            queue,
            key: KeyReport { id: lock.key_id(), params: lock.params(), locked_at: lock.locked_at(), changes },
            relay,
            access,
        }
    }
}

/// A refusal of the device's cursor, said as what happened to the stream.
fn diagnose(stream: &StreamName, cursor: &Cursor, e: crate::TransportError) -> String {
    let error = match crate::client::classify_read(e, stream, cursor) {
        Error::Transport(e) => return e.to_string(),
        other => other,
    };
    format!("{error} - State::forget lets the device start over with this stream")
}

fn short(text: &str) -> &str {
    &text[..8.min(text.len())]
}

fn plural<'a>(count: u64, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

/// A size for people: bytes, then KiB, MiB and GiB with one decimal.
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} {}", plural(bytes, "byte", "bytes")),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
        _ => format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0),
    }
}

/// A moment as a date and how long ago it was: `2026-10-09 (12 days ago)`.
fn when(moment: SystemTime, now: SystemTime) -> String {
    let secs = moment.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (year, month, day) = civil(secs / 86_400);
    let ago = match now.duration_since(moment).map(|d| d.as_secs() / 86_400) {
        Ok(0) | Err(_) => "today".to_string(),
        Ok(1) => "yesterday".to_string(),
        Ok(days) => format!("{days} days ago"),
    };
    format!("{year:04}-{month:02}-{day:02} ({ago})")
}

/// The calendar date of a day counted from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(queue: Result<Queue, String>) -> Report {
        let stream = StreamId::from_bytes([1; 16]);
        Report {
            transport: "http://relay.home.arpa".into(),
            stream: "notes".parse().unwrap(),
            stream_id: stream,
            device: DeviceId::from_bytes([2; 16]),
            epoch: Epoch(1),
            cursor: Ok(Cursor::start(stream)),
            queue,
            key: KeyReport {
                id: KeyId::from_bytes([3; 8]),
                params: KdfParams::DEFAULT,
                locked_at: SystemTime::now(),
                changes: 0,
            },
            relay: Ok(RelayReport { latency: Duration::from_millis(3), head: 0, epoch: Epoch(1) }),
            access: Ok(Limits::DEFAULT),
        }
    }

    #[test]
    fn a_queue_that_waited_over_a_day_is_a_problem() {
        let now = UNIX_EPOCH + Duration::from_secs(20_737 * 86_400);
        let waiting = |waited: Duration| Queue { batches: 1, items: 3, bytes: 2_500, oldest: Some(now - waited) };
        assert!(report(Ok(Queue::default())).problems_at(now).is_empty());
        assert!(report(Ok(waiting(QUEUE_STALE))).problems_at(now).is_empty(), "a day exactly is not over a day");
        let problems = report(Ok(waiting(QUEUE_STALE + Duration::from_secs(86_400)))).problems_at(now);
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].starts_with("3 items have waited to be sent since 2026-10-09 (2 days ago)"),
            "{problems:?}"
        );
        assert_eq!(report(Err("locked".into())).problems_at(now), ["the queue cannot be read: locked"]);
    }

    #[test]
    fn the_report_says_what_waits() {
        let empty = report(Ok(Queue::default())).to_string();
        assert!(empty.contains("\nqueue    empty\n"), "{empty}");
        let waiting = Queue { batches: 2, items: 3, bytes: 2_500, oldest: Some(SystemTime::now()) };
        let shown = report(Ok(waiting)).to_string();
        assert!(shown.contains("\nqueue    3 items (2.4 KiB) to send, the oldest since "), "{shown}");
        assert!(shown.contains("(today)"), "{shown}");
    }

    #[test]
    fn the_report_says_what_the_key_may_do_and_when_it_cannot_send() {
        let now = SystemTime::now();
        let shown = report(Ok(Queue::default())).to_string();
        assert!(shown.contains("\naccess   reads and writes - 0 bytes of 8.0 GiB in the space\n"), "{shown}");

        let waiting = Queue { batches: 1, items: 2, bytes: 100, oldest: Some(now) };
        let mut reader = report(Ok(waiting));
        reader.access = Ok(Limits { write: false, ..Limits::DEFAULT });
        assert!(reader.to_string().contains("\naccess   reads only - "), "{reader}");
        assert_eq!(
            reader.problems_at(now),
            ["2 items wait to be sent, and the access key only reads: the relay will not take them"]
        );
        reader.queue = Ok(Queue::default());
        assert!(reader.problems_at(now).is_empty(), "a key that reads and has nothing to send is no problem");

        let mut full = report(Ok(Queue::default()));
        full.access = Ok(Limits { used: 900, quota: 1000, ..Limits::DEFAULT });
        let problems = full.problems_at(now);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("the space holds 900 bytes of the 1000 bytes it may"), "{problems:?}");
        full.access = Ok(Limits { used: 899, quota: 1000, ..Limits::DEFAULT });
        assert!(full.problems_at(now).is_empty());
    }

    #[test]
    fn sizes_are_for_people() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(1023), "1023 bytes");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(5 * 1_048_576 / 2), "2.5 MiB");
        assert_eq!(size(8 << 30), "8.0 GiB");
    }

    #[test]
    fn dates_are_calendar_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_735), (2026, 10, 9));
        let moment = UNIX_EPOCH + Duration::from_secs(20_735 * 86_400 + 3600);
        assert_eq!(when(moment, moment + Duration::from_secs(12 * 86_400)), "2026-10-09 (12 days ago)");
        assert_eq!(when(moment, moment), "2026-10-09 (today)");
    }
}
