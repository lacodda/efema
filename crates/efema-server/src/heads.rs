//! Who is waiting for which stream, and the news that wakes them.
//!
//! One `watch` channel per stream - a name in a space - that anyone has
//! waited on or written to,
//! carrying the stream's identity and head. A write publishes the new head
//! after its commit; a waiting reader holds a receiver and wakes when the value
//! stops matching what it said it knew. The channel carries positions only -
//! a wait never touches entry data.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use efema_proto::{StreamId, StreamName};
use tokio::sync::watch;

use crate::store::StreamState;

/// A stream as the relay addresses it: its space, and its name there.
pub type Address = (i64, StreamName);

#[derive(Default)]
pub struct Heads {
    streams: Mutex<HashMap<Address, watch::Sender<StreamState>>>,
}

impl Heads {
    /// Records that `name` is now stream `id` with its last entry at `head`.
    ///
    /// Called by the writer after each commit, in commit order. The value only
    /// moves forward: a stale publication cannot pull a head back.
    pub fn publish(&self, name: &Address, id: StreamId, head: u64) {
        let state = StreamState { id: Some(id), head };
        let mut streams = self.lock();
        match streams.get(name) {
            Some(sender) => {
                sender.send_if_modified(|current| {
                    let newer = current.id != state.id || current.head < state.head;
                    if newer {
                        *current = state;
                    }
                    newer
                });
            }
            None => {
                streams.insert(name.clone(), watch::Sender::new(state));
            }
        }
    }

    /// A receiver for `name`, starting from `load()` if nobody has asked about
    /// the stream before.
    ///
    /// `load` reads the database while the map is held, which is what makes
    /// this race-free: a writer that commits meanwhile cannot publish until the
    /// map is released, and then it finds this receiver's channel and moves it
    /// forward. Without the lock a commit could land between the read and the
    /// insert, and the waiter would sleep through it.
    pub fn subscribe<E>(
        &self,
        name: &Address,
        load: impl FnOnce() -> Result<StreamState, E>,
    ) -> Result<watch::Receiver<StreamState>, E> {
        let mut streams = self.lock();
        if let Some(sender) = streams.get(name) {
            return Ok(sender.subscribe());
        }
        let sender = watch::Sender::new(load()?);
        let receiver = sender.subscribe();
        streams.insert(name.clone(), sender);
        Ok(receiver)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Address, watch::Sender<StreamState>>> {
        // Nothing inside the map can be left half-updated by a panic: every
        // change is a single insert or a single send.
        self.streams.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> Address {
        (1, "s".parse().unwrap())
    }

    #[test]
    fn a_head_never_goes_back() {
        let heads = Heads::default();
        let id = StreamId::from_bytes([1; 16]);
        heads.publish(&name(), id, 5);
        let rx = heads.subscribe::<()>(&name(), || unreachable!("the channel exists")).unwrap();
        heads.publish(&name(), id, 3);
        assert_eq!(rx.borrow().head, 5);
        heads.publish(&name(), id, 7);
        assert_eq!(rx.borrow().head, 7);
    }

    #[test]
    fn the_first_subscriber_loads_the_state() {
        let heads = Heads::default();
        let rx = heads.subscribe::<()>(&name(), || Ok(StreamState { id: None, head: 0 })).unwrap();
        assert_eq!(*rx.borrow(), StreamState::default());
        heads.publish(&name(), StreamId::from_bytes([2; 16]), 1);
        assert!(rx.has_changed().unwrap());
    }

    #[test]
    fn a_name_in_another_space_is_another_stream() {
        let heads = Heads::default();
        let other: Address = (2, "s".parse().unwrap());
        let rx = heads.subscribe::<()>(&other, || Ok(StreamState::default())).unwrap();
        heads.publish(&name(), StreamId::from_bytes([2; 16]), 1);
        assert!(!rx.has_changed().unwrap(), "a write in one space woke a waiter in another");
    }
}
