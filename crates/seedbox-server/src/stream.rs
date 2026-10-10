//! Bounded live delivery. Publication never performs client I/O and a reader
//! cannot hold the publisher back. New subscriptions have no historical data.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

/// A record must account for its variable-sized owned data. The stream bounds
/// both the number of retained records and each record's payload size.
pub trait Payload: Send + Sync + 'static {
    fn payload_bytes(&self) -> usize;
}

#[derive(Debug)]
pub struct Record<T> {
    /// Delivery sequence, unrelated to a resource revision or timeline position.
    pub sequence: u64,
    pub payload: T,
}

#[derive(Debug)]
pub enum Delivery<T> {
    Record(Arc<Record<T>>),
    /// Inclusive sequence range discarded before this subscriber could read it.
    Gap {
        first: u64,
        last: u64,
    },
    /// No further observations will be published. Inspect the resource for its
    /// final status; end of a stream by itself says nothing about test success.
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("observation exceeds the {limit}-byte stream payload limit")]
    TooLarge { limit: usize },
    #[error("observation stream is closed")]
    Closed,
    #[error("observation stream sequence is exhausted")]
    SequenceExhausted,
    #[error("observation stream publisher failed")]
    Poisoned,
}

struct State<T> {
    sender: Option<broadcast::Sender<Arc<Record<T>>>>,
    next_sequence: u64,
}

/// Each simulation owns separate log and event streams. A short critical
/// section orders publication and subscription; no subscriber runs under it.
pub struct Stream<T> {
    state: Mutex<State<T>>,
    max_payload_bytes: NonZeroUsize,
}

pub struct Subscription<T> {
    receiver: Option<broadcast::Receiver<Arc<Record<T>>>>,
    next_sequence: u64,
}

impl<T: Payload> Stream<T> {
    pub fn new(capacity: NonZeroUsize, max_payload_bytes: NonZeroUsize) -> Self {
        let (sender, _) = broadcast::channel(capacity.get());
        Self {
            state: Mutex::new(State {
                sender: Some(sender),
                next_sequence: 1,
            }),
            max_payload_bytes,
        }
    }

    pub fn subscribe(&self) -> Result<Subscription<T>, PublishError> {
        let state = self.state.lock().map_err(|_| PublishError::Poisoned)?;
        Ok(Subscription {
            receiver: state.sender.as_ref().map(|sender| sender.subscribe()),
            next_sequence: state.next_sequence,
        })
    }

    pub fn publish(&self, payload: T) -> Result<u64, PublishError> {
        if payload.payload_bytes() > self.max_payload_bytes.get() {
            return Err(PublishError::TooLarge {
                limit: self.max_payload_bytes.get(),
            });
        }
        let mut state = self.state.lock().map_err(|_| PublishError::Poisoned)?;
        let sender = state.sender.as_ref().ok_or(PublishError::Closed)?;
        let sequence = state.next_sequence;
        let next = sequence
            .checked_add(1)
            .ok_or(PublishError::SequenceExhausted)?;
        // No readers is normal. Sending does not await them or retain a backlog
        // for future readers, and receiver count has no effect on sequencing.
        let _ = sender.send(Arc::new(Record { sequence, payload }));
        state.next_sequence = next;
        Ok(sequence)
    }

    /// Existing readers drain their bounded buffers, then see Closed. Later
    /// subscribers immediately see Closed. Closing is idempotent.
    pub fn close(&self) -> Result<(), PublishError> {
        let mut state = self.state.lock().map_err(|_| PublishError::Poisoned)?;
        state.sender.take();
        Ok(())
    }
}

impl<T> Subscription<T> {
    pub fn closed() -> Self {
        Self {
            receiver: None,
            next_sequence: 1,
        }
    }

    pub async fn recv(&mut self) -> Delivery<T> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Delivery::Closed;
        };
        match receiver.recv().await {
            Ok(record) => {
                // Publishers stop before overflow, so every published sequence
                // has a representable successor.
                self.next_sequence = record.sequence + 1;
                Delivery::Record(record)
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                let first = self.next_sequence;
                self.next_sequence += count;
                Delivery::Gap {
                    first,
                    last: self.next_sequence - 1,
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                self.receiver = None;
                Delivery::Closed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Payload for Vec<u8> {
        fn payload_bytes(&self) -> usize {
            self.len()
        }
    }

    fn stream() -> Stream<Vec<u8>> {
        Stream::new(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(8).unwrap())
    }

    #[tokio::test]
    async fn slow_reader_has_an_exact_gap_while_fast_reader_keeps_up() {
        let stream = stream();
        let mut slow = stream.subscribe().unwrap();
        let mut fast = stream.subscribe().unwrap();
        for byte in 1..=5 {
            stream.publish(vec![byte]).unwrap();
            let Delivery::Record(record) = fast.recv().await else {
                panic!("fast reader lost data")
            };
            assert_eq!(record.sequence, u64::from(byte));
        }
        assert!(matches!(
            slow.recv().await,
            Delivery::Gap { first: 1, last: 3 }
        ));
        stream.close().unwrap();
        for expected in [4, 5] {
            let Delivery::Record(record) = slow.recv().await else {
                panic!("missing retained record")
            };
            assert_eq!(record.sequence, expected);
        }
        assert!(matches!(slow.recv().await, Delivery::Closed));
        assert!(matches!(fast.recv().await, Delivery::Closed));
    }

    #[tokio::test]
    async fn new_subscriber_gets_no_backlog_and_binary_payloads_are_unchanged() {
        let stream = stream();
        stream.publish(vec![1]).unwrap();
        let mut reader = stream.subscribe().unwrap();
        let bytes = vec![0, 255, b'\n', 0xc3];
        stream.publish(bytes.clone()).unwrap();
        let Delivery::Record(record) = reader.recv().await else {
            panic!("missing live record")
        };
        assert_eq!(record.sequence, 2);
        assert_eq!(record.payload, bytes);
        stream.close().unwrap();
        assert!(matches!(
            stream.subscribe().unwrap().recv().await,
            Delivery::Closed
        ));
        assert!(matches!(stream.publish(vec![]), Err(PublishError::Closed)));
    }

    #[test]
    fn oversized_payload_is_rejected_without_consuming_a_sequence() {
        let stream = stream();
        assert!(matches!(
            stream.publish(vec![0; 9]),
            Err(PublishError::TooLarge { limit: 8 })
        ));
        assert_eq!(stream.publish(vec![0; 8]).unwrap(), 1);
    }
}
