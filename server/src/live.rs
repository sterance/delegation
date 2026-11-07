//! Live, in-memory state: which clients are connected, each one's recent
//! metric samples, and `last_seen_at` values not yet flushed to SQLite.
//!
//! None of the samples are ever persisted. Restarting the server starts
//! with no connections and no history, and a new `epoch` tells the
//! dashboard its sequence numbers have reset.

use crate::config::MetricsLimits;
use crate::metrics;
use protocol::{GpuMetrics, Metrics};
use rand::Rng;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};
use tokio::sync::Notify;

/// One accepted sample, as `GET /api/clients` returns it.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Sample {
    /// Server-wide: one counter across all clients, +1 per accepted sample.
    pub seq: u64,
    /// When the server received the tick, with milliseconds.
    pub received_at: String,
    pub cpu_percent: f32,
    pub ram_total_bytes: u64,
    pub ram_available_bytes: u64,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
    pub gpus: Vec<GpuMetrics>,
}

/// Handed to a connection task when it registers. `kick` fires when the
/// connection must close: the same client connected again, or it was
/// deleted from the dashboard.
pub struct ConnHandle {
    pub id: u64,
    pub kick: Arc<Notify>,
}

#[derive(Debug, PartialEq)]
pub enum SampleOutcome {
    Accepted {
        seq: u64,
    },
    Rejected(String),
    /// The connection was replaced or kicked while this tick was in flight.
    NotConnected,
}

/// What `GET /api/clients` needs, read under one lock so `latest_seq` and the
/// samples agree: a sample is never newer than the cursor the dashboard
/// gets back, so it can't be skipped on the next poll.
pub struct Snapshot {
    pub latest_seq: u64,
    pub connected: HashMap<String, Vec<Sample>>,
    pub last_seen: HashMap<String, String>,
}

pub struct Live {
    /// Random per process start.
    pub epoch: String,
    limits: MetricsLimits,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    conns: HashMap<String, Conn>,
    last_seen: HashMap<String, LastSeen>,
    latest_seq: u64,
    next_conn_id: u64,
}

struct Conn {
    id: u64,
    kick: Arc<Notify>,
    samples: VecDeque<Sample>,
    last_accepted: Option<Instant>,
}

struct LastSeen {
    at: SystemTime,
    /// Changed since it was last written to SQLite.
    dirty: bool,
}

impl Live {
    pub fn new(limits: MetricsLimits) -> Self {
        let bytes: [u8; 8] = rand::thread_rng().gen();
        Live {
            epoch: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            limits,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Nothing here can panic mid-update, but don't let a poisoned lock
        // take the whole server down if something ever does.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks the client connected with an empty history. A connection the
    /// same client already had is kicked: the client only ever reconnects
    /// after giving up on the old one, even if the server hasn't noticed yet.
    pub fn register(&self, client_id: &str) -> ConnHandle {
        let mut inner = self.lock();
        inner.next_conn_id += 1;
        let id = inner.next_conn_id;
        let kick = Arc::new(Notify::new());
        let old = inner.conns.insert(
            client_id.to_string(),
            Conn {
                id,
                kick: kick.clone(),
                samples: VecDeque::with_capacity(self.limits.history_samples),
                last_accepted: None,
            },
        );
        if let Some(old) = old {
            // notify_one stores a permit, so this lands even if the old task
            // isn't waiting on it right now.
            old.kick.notify_one();
        }
        touch(&mut inner, client_id);
        ConnHandle { id, kick }
    }

    /// Removes the connection and its history, if `conn_id` is still the
    /// client's current connection (a replaced one mustn't remove its
    /// successor). Returns whether it did.
    pub fn unregister(&self, client_id: &str, conn_id: u64) -> bool {
        let mut inner = self.lock();
        if inner.conns.get(client_id).is_some_and(|c| c.id == conn_id) {
            inner.conns.remove(client_id);
            true
        } else {
            false
        }
    }

    /// Any inbound frame: proof of life.
    pub fn touch(&self, client_id: &str) {
        touch(&mut self.lock(), client_id);
    }

    pub fn record_sample(&self, client_id: &str, conn_id: u64, m: Metrics, now: Instant) -> SampleOutcome {
        if let Err(reason) = metrics::validate(&m, &self.limits) {
            return SampleOutcome::Rejected(reason);
        }
        let mut inner = self.lock();
        let Inner { conns, latest_seq, .. } = &mut *inner;
        let Some(conn) = conns.get_mut(client_id).filter(|c| c.id == conn_id) else {
            return SampleOutcome::NotConnected;
        };
        if let Some(prev) = conn.last_accepted {
            let gap = now.saturating_duration_since(prev);
            if gap < self.limits.min_spacing {
                return SampleOutcome::Rejected(format!(
                    "arrived {} ms after the previous sample, less than {} ms",
                    gap.as_millis(),
                    self.limits.min_spacing.as_millis()
                ));
            }
        }
        *latest_seq += 1;
        let seq = *latest_seq;
        conn.last_accepted = Some(now);
        if conn.samples.len() >= self.limits.history_samples {
            conn.samples.pop_front();
        }
        conn.samples.push_back(Sample {
            seq,
            received_at: protocol::utc_timestamp(SystemTime::now(), true),
            cpu_percent: m.cpu_percent,
            ram_total_bytes: m.ram_total_bytes,
            ram_available_bytes: m.ram_available_bytes,
            swap_total_bytes: m.swap_total_bytes,
            swap_used_bytes: m.swap_used_bytes,
            gpus: m.gpus,
        });
        SampleOutcome::Accepted { seq }
    }

    /// Samples with `seq > since` for every connected client, oldest first.
    pub fn snapshot(&self, since: u64) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            latest_seq: inner.latest_seq,
            connected: inner
                .conns
                .iter()
                .map(|(id, conn)| {
                    let samples = conn.samples.iter().filter(|s| s.seq > since).cloned().collect();
                    (id.clone(), samples)
                })
                .collect(),
            last_seen: inner
                .last_seen
                .iter()
                .map(|(id, seen)| (id.clone(), protocol::utc_timestamp(seen.at, false)))
                .collect(),
        }
    }

    /// The deleted-client path: closes its connection (if any) and drops its
    /// unflushed `last_seen_at`.
    pub fn forget(&self, client_id: &str) {
        let mut inner = self.lock();
        inner.last_seen.remove(client_id);
        if let Some(conn) = inner.conns.get(client_id) {
            conn.kick.notify_one();
        }
    }

    /// Every `last_seen_at` changed since the last flush, as (client_id,
    /// timestamp), marking them clean. `only` limits it to one client.
    pub fn take_dirty(&self, only: Option<&str>) -> Vec<(String, String)> {
        let mut inner = self.lock();
        inner
            .last_seen
            .iter_mut()
            .filter(|(id, seen)| seen.dirty && only.is_none_or(|only| only == id.as_str()))
            .map(|(id, seen)| {
                seen.dirty = false;
                (id.clone(), protocol::utc_timestamp(seen.at, false))
            })
            .collect()
    }

    /// Puts back rows a failed flush didn't write, so the next one retries.
    pub fn mark_dirty(&self, rows: &[(String, String)]) {
        let mut inner = self.lock();
        for (id, _) in rows {
            if let Some(seen) = inner.last_seen.get_mut(id) {
                seen.dirty = true;
            }
        }
    }
}

fn touch(inner: &mut Inner, client_id: &str) {
    inner.last_seen.insert(
        client_id.to_string(),
        LastSeen {
            at: SystemTime::now(),
            dirty: true,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::time::Duration;

    fn live(history: usize) -> Live {
        Live::new(MetricsLimits {
            history_samples: history,
            min_spacing: Duration::from_millis(500),
            max_gpus: 16,
            max_string_len: 128,
        })
    }

    fn metrics(cpu: f32) -> Metrics {
        Metrics {
            sample_age_ms: 0,
            cpu_percent: cpu,
            ram_total_bytes: 10,
            ram_available_bytes: 5,
            swap_total_bytes: None,
            swap_used_bytes: None,
            gpus: vec![],
        }
    }

    #[test]
    fn ring_buffer_trims_and_seq_is_server_wide() {
        let live = live(3);
        let a = live.register("a");
        let b = live.register("b");
        let t0 = Instant::now();
        for i in 0..5u32 {
            let t = t0 + Duration::from_secs(i.into());
            assert!(matches!(
                live.record_sample("a", a.id, metrics(i as f32), t),
                SampleOutcome::Accepted { .. }
            ));
            assert!(matches!(
                live.record_sample("b", b.id, metrics(50.0), t),
                SampleOutcome::Accepted { .. }
            ));
        }
        let snap = live.snapshot(0);
        assert_eq!(snap.latest_seq, 10);
        let a_samples = &snap.connected["a"];
        assert_eq!(a_samples.len(), 3, "trimmed to METRICS_HISTORY_SAMPLES");
        assert_eq!(a_samples.iter().map(|s| s.seq).collect::<Vec<_>>(), vec![5, 7, 9]);
        assert_eq!(a_samples[2].cpu_percent, 4.0);
        assert!(a_samples[0].received_at.ends_with('Z') && a_samples[0].received_at.contains('.'));

        let since = live.snapshot(8);
        assert_eq!(since.connected["a"].iter().map(|s| s.seq).collect::<Vec<_>>(), vec![9]);
        assert_eq!(since.connected["b"].iter().map(|s| s.seq).collect::<Vec<_>>(), vec![10]);
        assert!(live.snapshot(10).connected["a"].is_empty());
    }

    #[test]
    fn samples_closer_than_min_spacing_are_dropped() {
        let live = live(60);
        let c = live.register("c");
        let t0 = Instant::now();
        assert!(matches!(
            live.record_sample("c", c.id, metrics(1.0), t0),
            SampleOutcome::Accepted { seq: 1 }
        ));
        let too_soon = live.record_sample("c", c.id, metrics(2.0), t0 + Duration::from_millis(499));
        assert!(matches!(too_soon, SampleOutcome::Rejected(r) if r.contains("less than 500 ms")));
        // Measured from the last *accepted* sample.
        assert!(matches!(
            live.record_sample("c", c.id, metrics(3.0), t0 + Duration::from_millis(500)),
            SampleOutcome::Accepted { seq: 2 }
        ));
    }

    #[test]
    fn invalid_samples_are_rejected_without_using_a_seq() {
        let live = live(60);
        let c = live.register("c");
        assert!(matches!(
            live.record_sample("c", c.id, metrics(f32::NAN), Instant::now()),
            SampleOutcome::Rejected(_)
        ));
        assert_eq!(live.snapshot(0).latest_seq, 0);
        assert!(live.snapshot(0).connected["c"].is_empty());
    }

    #[test]
    fn reconnect_kicks_the_old_connection_and_disconnect_clears_history() {
        let live = live(60);
        let first = live.register("c");
        live.record_sample("c", first.id, metrics(1.0), Instant::now());
        let second = live.register("c");
        // The old connection's kick fired, and it can no longer add samples
        // or unregister its successor.
        assert!(futures_now(first.kick.notified()));
        assert_eq!(
            live.record_sample("c", first.id, metrics(1.0), Instant::now()),
            SampleOutcome::NotConnected
        );
        assert!(!live.unregister("c", first.id));
        assert!(
            live.snapshot(0).connected["c"].is_empty(),
            "new connection starts empty"
        );
        assert!(live.unregister("c", second.id));
        assert!(!live.snapshot(0).connected.contains_key("c"));
    }

    #[test]
    fn last_seen_is_tracked_dirty_until_taken() {
        let live = live(60);
        let _c = live.register("c");
        live.touch("d");
        assert!(live.snapshot(0).last_seen.contains_key("c"));

        let only_c = live.take_dirty(Some("c"));
        assert_eq!(only_c.len(), 1);
        assert_eq!(only_c[0].0, "c");
        let rest = live.take_dirty(None);
        assert_eq!(rest.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["d"]);
        assert!(live.take_dirty(None).is_empty(), "nothing changed since");

        live.mark_dirty(&rest);
        assert_eq!(live.take_dirty(None).len(), 1, "a failed flush is retried");
        live.touch("c");
        assert_eq!(live.take_dirty(None).len(), 1);

        live.forget("c");
        assert!(!live.snapshot(0).last_seen.contains_key("c"));
    }

    /// Whether a `Notified` future is already complete (a stored permit).
    fn futures_now(fut: tokio::sync::futures::Notified<'_>) -> bool {
        let mut fut = std::pin::pin!(fut);
        let waker = std::task::Waker::noop();
        fut.as_mut().poll(&mut std::task::Context::from_waker(waker)).is_ready()
    }
}
