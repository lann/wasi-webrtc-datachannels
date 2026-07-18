//! In-memory room/mailbox state for the conformance signaling server.
//!
//! A [`Rooms`] store holds independent rooms addressed by an opaque id; each
//! room has two [`Mailbox`]es (one per [`Role`]). Peers publish blobs to their
//! own role's mailbox and fetch the peer's mailbox by sequence number. Fetches
//! are idempotent and may long-poll: [`Mailbox`] exposes a [`tokio::sync::Notify`]
//! that wakes blocked fetches when a blob is published, the mailbox is marked
//! done, or the room is deleted.
//!
//! See `conformance/signaling/PROTOCOL.md` for the wire contract this backs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{Notify, RwLock};

/// A signaling role. A room has exactly one mailbox per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Offerer,
    Answerer,
}

impl Role {
    /// Parse the path token (`offerer` / `answerer`).
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "offerer" => Some(Role::Offerer),
            "answerer" => Some(Role::Answerer),
            _ => None,
        }
    }
}

/// Outcome of a (possibly long-polling) fetch of a single sequence number.
#[derive(Debug, Clone)]
pub enum Fetch {
    /// The blob at the requested seq is available.
    Blob(Bytes),
    /// The mailbox is done and the requested seq is at/past its frozen length:
    /// no such blob will ever exist (maps to `recv() -> none`).
    Done,
    /// The blob is not yet available and the mailbox is not done: the caller
    /// should retry the same seq (maps to HTTP `304`).
    Pending,
}

/// One append-only, ordered mailbox with a `done` flag.
#[derive(Debug, Default)]
struct Mailbox {
    blobs: Vec<Bytes>,
    done: bool,
    /// Woken whenever `blobs` grows, `done` is set, or the room is removed.
    notify: Arc<Notify>,
}

impl Mailbox {
    fn wake(&self) {
        self.notify.notify_waiters();
    }

    /// Classify a fetch of `seq` against the current mailbox contents.
    fn peek(&self, seq: usize) -> Fetch {
        if let Some(blob) = self.blobs.get(seq) {
            Fetch::Blob(blob.clone())
        } else if self.done {
            Fetch::Done
        } else {
            Fetch::Pending
        }
    }
}

/// A room: two mailboxes plus a last-touched timestamp for TTL eviction.
#[derive(Debug)]
struct Room {
    offerer: Mailbox,
    answerer: Mailbox,
    last_touched: Instant,
}

impl Room {
    fn new(now: Instant) -> Self {
        Room {
            offerer: Mailbox::default(),
            answerer: Mailbox::default(),
            last_touched: now,
        }
    }

    fn mailbox(&self, role: Role) -> &Mailbox {
        match role {
            Role::Offerer => &self.offerer,
            Role::Answerer => &self.answerer,
        }
    }

    fn mailbox_mut(&mut self, role: Role) -> &mut Mailbox {
        match role {
            Role::Offerer => &mut self.offerer,
            Role::Answerer => &mut self.answerer,
        }
    }
}

/// Result of a publish attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// The blob was appended with this zero-based sequence number.
    Seq(usize),
    /// The mailbox is already done; publishing is rejected.
    Done,
    /// A capacity cap was exceeded.
    Capacity,
}

/// Configuration limits applied by the store.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Room TTL: a room is evicted this long after its last activity.
    pub room_ttl: Duration,
    /// Maximum number of live rooms (0 = unlimited).
    pub max_rooms: usize,
    /// Maximum blobs per mailbox (0 = unlimited).
    pub max_blobs_per_mailbox: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            room_ttl: Duration::from_secs(300),
            max_rooms: 0,
            max_blobs_per_mailbox: 0,
        }
    }
}

/// The in-memory room store. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Rooms {
    inner: Arc<RwLock<HashMap<String, Room>>>,
    limits: Limits,
}

impl Rooms {
    pub fn new(limits: Limits) -> Self {
        Rooms {
            inner: Arc::new(RwLock::new(HashMap::new())),
            limits,
        }
    }

    /// Append `blob` to `role`'s mailbox in `room`, creating the room if needed.
    pub async fn publish(&self, room: &str, role: Role, blob: Bytes) -> Publish {
        let now = Instant::now();
        let mut map = self.inner.write().await;

        if !map.contains_key(room)
            && self.limits.max_rooms != 0
            && map.len() >= self.limits.max_rooms
        {
            return Publish::Capacity;
        }

        let entry = map
            .entry(room.to_string())
            .or_insert_with(|| Room::new(now));
        entry.last_touched = now;
        let mailbox = entry.mailbox_mut(role);

        if mailbox.done {
            return Publish::Done;
        }
        if self.limits.max_blobs_per_mailbox != 0
            && mailbox.blobs.len() >= self.limits.max_blobs_per_mailbox
        {
            return Publish::Capacity;
        }

        let seq = mailbox.blobs.len();
        mailbox.blobs.push(blob);
        mailbox.wake();
        Publish::Seq(seq)
    }

    /// Mark `role`'s mailbox in `room` done; returns the frozen blob count.
    pub async fn mark_done(&self, room: &str, role: Role) -> usize {
        let now = Instant::now();
        let mut map = self.inner.write().await;
        let entry = map
            .entry(room.to_string())
            .or_insert_with(|| Room::new(now));
        entry.last_touched = now;
        let mailbox = entry.mailbox_mut(role);
        mailbox.done = true;
        let len = mailbox.blobs.len();
        mailbox.wake();
        len
    }

    /// Non-blocking classification of a fetch of `role`'s mailbox at `seq`.
    ///
    /// Returns the current [`Fetch`] plus the mailbox's [`Notify`] so a caller
    /// that observes [`Fetch::Pending`] can await a wakeup and re-peek. The
    /// room is created if needed so a consumer can start before the publisher.
    pub async fn fetch_now(&self, room: &str, role: Role, seq: usize) -> (Fetch, Arc<Notify>) {
        let now = Instant::now();
        let mut map = self.inner.write().await;
        let entry = map
            .entry(room.to_string())
            .or_insert_with(|| Room::new(now));
        entry.last_touched = now;
        let mailbox = entry.mailbox(role);
        (mailbox.peek(seq), mailbox.notify.clone())
    }

    /// Long-poll a fetch of `role`'s mailbox at `seq` for up to `wait`.
    ///
    /// Resolves immediately on [`Fetch::Blob`]/[`Fetch::Done`]; on
    /// [`Fetch::Pending`] it awaits a mailbox wakeup and re-peeks, returning
    /// [`Fetch::Pending`] if `wait` elapses first. `wait == 0` is a pure
    /// non-blocking peek.
    pub async fn fetch(&self, room: &str, role: Role, seq: usize, wait: Duration) -> Fetch {
        let deadline = Instant::now() + wait;
        loop {
            let (outcome, notify) = self.fetch_now(room, role, seq).await;
            match outcome {
                Fetch::Blob(_) | Fetch::Done => return outcome,
                Fetch::Pending => {}
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Fetch::Pending;
            }

            // Register for notification, then re-check under the notified
            // future's presence to avoid missing a wake between peek and await.
            let notified = notify.notified();
            tokio::pin!(notified);
            // Re-peek once more in case a blob landed between the peek above and
            // arming `notified`.
            let (recheck, _) = self.fetch_now(room, role, seq).await;
            match recheck {
                Fetch::Blob(_) | Fetch::Done => return recheck,
                Fetch::Pending => {}
            }

            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return Fetch::Pending;
            }
        }
    }

    /// Delete a room, waking any blocked fetches. Returns whether it existed.
    pub async fn delete(&self, room: &str) -> bool {
        let mut map = self.inner.write().await;
        if let Some(r) = map.remove(room) {
            r.offerer.wake();
            r.answerer.wake();
            true
        } else {
            false
        }
    }

    /// Evict rooms untouched for longer than the configured TTL, waking their
    /// blocked fetches. Returns the number of rooms evicted.
    pub async fn evict_expired(&self) -> usize {
        let now = Instant::now();
        let ttl = self.limits.room_ttl;
        let mut map = self.inner.write().await;
        let expired: Vec<String> = map
            .iter()
            .filter(|(_, r)| now.duration_since(r.last_touched) > ttl)
            .map(|(k, _)| k.clone())
            .collect();
        for key in &expired {
            if let Some(r) = map.remove(key) {
                r.offerer.wake();
                r.answerer.wake();
            }
        }
        expired.len()
    }

    /// Current live room count (test/introspection helper).
    pub async fn room_count(&self) -> usize {
        self.inner.read().await.len()
    }
}
