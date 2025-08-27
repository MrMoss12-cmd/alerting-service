// src/repository/retry_queue.rs
//! Retry queue (DLQ / backoff queue) implementation.
//!
//! Responsibilities:
//! - Persist entries that failed delivery and must be retried later.
//! - Support multiple retry policies (exponential, linear, fixed).
//! - Tenant isolation (queues per tenant).
//! - Prioritization by severity/SLA and next attempt time.
//! - Expiration of old entries.
//! - Metrics hooks for monitoring.
//!
//! This file provides:
//! - `RetryPolicy` enum with parameters.
//! - `RetryEntry` struct representing queued item (serializable).
//! - `RetryQueue` trait describing operations.
//! - `InMemoryRetryQueue` implementation suitable for tests/dev.
//! - (Optional) Postgres-backed implementation placeholder under feature "postgres".
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BinaryHeap, HashMap},
    cmp::Ordering,
    sync::Arc,
};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Severity-like priority used for ordering (higher -> more urgent)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    Low = 0,
    Medium = 50,
    High = 100,
    Critical = 200,
    Custom(i64),
}

impl From<i64> for Priority {
    fn from(v: i64) -> Self {
        match v {
            x if x >= 200 => Priority::Custom(x),
            100 => Priority::Critical,
            50 => Priority::Medium,
            0 => Priority::Low,
            _ => Priority::Custom(v),
        }
    }
}

/// Retry policies supported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RetryPolicy {
    /// exponential backoff: initial, multiplier, max_interval_seconds, max_retries
    Exponential {
        initial_seconds: u64,
        multiplier: f64,
        max_interval_seconds: u64,
        max_retries: u32,
    },
    /// linear backoff: initial, step_seconds, max_retries
    Linear {
        initial_seconds: u64,
        step_seconds: u64,
        max_retries: u32,
    },
    /// fixed interval: every N seconds, max_retries
    Fixed {
        interval_seconds: u64,
        max_retries: u32,
    },
}

impl RetryPolicy {
    /// Compute next interval (seconds) given attempt_count (0-based)
    pub fn next_interval_seconds(&self, attempt_count: u32) -> u64 {
        match self {
            RetryPolicy::Exponential { initial_seconds, multiplier, max_interval_seconds, .. } => {
                let base = (*initial_seconds as f64) * multiplier.powi(attempt_count as i32);
                let v = base.min(*max_interval_seconds as f64);
                v as u64
            }
            RetryPolicy::Linear { initial_seconds, step_seconds, .. } => {
                initial_seconds + step_seconds.saturating_mul(attempt_count as u64)
            }
            RetryPolicy::Fixed { interval_seconds, .. } => *interval_seconds,
        }
    }

    pub fn max_retries(&self) -> u32 {
        match self {
            RetryPolicy::Exponential { max_retries, .. } => *max_retries,
            RetryPolicy::Linear { max_retries, .. } => *max_retries,
            RetryPolicy::Fixed { max_retries, .. } => *max_retries,
        }
    }
}

/// An entry stored in the retry queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryEntry {
    pub id: Uuid,
    pub tenant_id: String,
    pub alert_id: Uuid,
    pub payload: serde_json::Value, // minimal safe copy of data required for retry
    pub priority: Priority,
    pub attempts: u32,
    pub first_failed_at: DateTime<Utc>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub next_attempt_at: DateTime<Utc>,
    pub policy: RetryPolicy,
    pub expires_at: Option<DateTime<Utc>>, // optional TTL for entry
}

/// Internal item ordering for BinaryHeap (min-heap by next_attempt_at then highest priority)
#[derive(Debug, Clone)]
struct HeapItem {
    next_attempt_at: DateTime<Utc>,
    priority: Priority,
    id: Uuid,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.next_attempt_at == other.next_attempt_at && self.priority == other.priority && self.id == other.id
    }
}
impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        // we want a min-heap by next_attempt_at, so reverse the cmp for BinaryHeap
        // and prefer higher priority when times equal
        Some(other.next_attempt_at.cmp(&self.next_attempt_at)
            .then_with(|| other.priority.cmp(&self.priority))
            .then_with(|| other.id.cmp(&self.id)))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

/// Trait for retry queue operations.
#[async_trait]
pub trait RetryQueue: Send + Sync + 'static {
    /// Enqueue a new retry entry. Returns entry id.
    async fn enqueue(&self, entry: RetryEntry) -> anyhow::Result<Uuid>;

    /// Peek ready entries (up to limit) that are due for retry (next_attempt_at <= now).
    /// This should not remove them; use `pop_ready` to claim for processing.
    async fn peek_ready(&self, tenant_id: &str, limit: usize) -> anyhow::Result<Vec<RetryEntry>>;

    /// Pop and claim ready entries (atomic for in-memory impl). Returns claimed entries.
    async fn pop_ready(&self, tenant_id: &str, limit: usize) -> anyhow::Result<Vec<RetryEntry>>;

    /// Mark a processing attempt: success -> remove; failure -> reschedule according to policy.
    /// returns Ok(true) if removed (success or expired), Ok(false) if rescheduled.
    async fn mark_attempt(&self, entry_id: &Uuid, success: bool, error_reason: Option<String>) -> anyhow::Result<bool>;

    /// Remove entry explicitly (e.g. canceled).
    async fn remove(&self, entry_id: &Uuid) -> anyhow::Result<()>;

    /// Purge expired entries (older than now or past expires_at). Returns number purged.
    async fn purge_expired(&self) -> anyhow::Result<usize>;

    /// Get stats for tenant (queue length, ready count)
    async fn stats(&self, tenant_id: &str) -> anyhow::Result<RetryQueueStats>;
}

#[derive(Debug, Clone)]
pub struct RetryQueueStats {
    pub tenant_id: String,
    pub queue_length: usize,
    pub ready_count: usize,
}

/// --------------------
/// In-memory implementation
/// --------------------
pub struct InMemoryRetryQueue {
    // per-tenant map of entries
    entries: Arc<RwLock<HashMap<Uuid, RetryEntry>>>,
    // per-tenant heaps
    heaps: Arc<RwLock<HashMap<String, BinaryHeap<HeapItem>>>>,
}

impl InMemoryRetryQueue {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            heaps: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// internal helper to push heap item for tenant
    async fn push_heap(&self, tenant_id: &str, item: HeapItem) {
        let mut heaps = self.heaps.write().await;
        let heap = heaps.entry(tenant_id.to_string()).or_insert_with(BinaryHeap::new);
        heap.push(item);
    }

    /// internal helper to remove from heap lazily (we don't support arbitrary removal efficiently)
    /// The pop/claim logic will skip stale heap entries if the corresponding entry has been removed or next_attempt changed.
}

#[async_trait]
impl RetryQueue for InMemoryRetryQueue {
    async fn enqueue(&self, mut entry: RetryEntry) -> anyhow::Result<Uuid> {
        let id = entry.id;
        let tenant = entry.tenant_id.clone();
        {
            let mut entries = self.entries.write().await;
            entries.insert(id, entry.clone());
        }
        // push to heap
        self.push_heap(&tenant, HeapItem {
            next_attempt_at: entry.next_attempt_at,
            priority: entry.priority,
            id,
        }).await;

        metrics::increment_counter!("retry_queue_enqueue");
        Ok(id)
    }

    async fn peek_ready(&self, tenant_id: &str, limit: usize) -> anyhow::Result<Vec<RetryEntry>> {
        let now = Utc::now();
        let heaps = self.heaps.read().await;
        let heap_opt = heaps.get(tenant_id);
        if heap_opt.is_none() {
            return Ok(vec![]);
        }
        let heap = heap_opt.unwrap();
        let mut out = Vec::new();
        // expensive but OK for prototype: iterate entries and find due ones ordered by next_attempt
        let entries = self.entries.read().await;
        let mut candidates: Vec<&RetryEntry> = entries.values()
            .filter(|e| e.tenant_id == tenant_id && e.next_attempt_at <= now)
            .collect();
        // sort by next_attempt then priority desc
        candidates.sort_by(|a, b| a.next_attempt_at.cmp(&b.next_attempt_at).then(b.priority.cmp(&a.priority)));
        for e in candidates.into_iter().take(limit) {
            out.push(e.clone());
        }
        Ok(out)
    }

    async fn pop_ready(&self, tenant_id: &str, limit: usize) -> anyhow::Result<Vec<RetryEntry>> {
        let now = Utc::now();
        let mut claimed = Vec::new();

        // We'll repeatedly pop from heap and validate entries
        let mut heaps = self.heaps.write().await;
        let heap = heaps.entry(tenant_id.to_string()).or_insert_with(BinaryHeap::new);

        while claimed.len() < limit {
            if let Some(item) = heap.peek().cloned() {
                // check entry exists and still due
                let mut entries = self.entries.write().await;
                if let Some(e) = entries.get(&item.id) {
                    if e.next_attempt_at <= now {
                        // claim: remove from heap and from entries map (temporarily)
                        heap.pop();
                        if let Some(entry) = entries.remove(&item.id) {
                            claimed.push(entry);
                            metrics::increment_counter!("retry_queue_claimed");
                            continue;
                        } else {
                            // removed concurrently, skip
                            continue;
                        }
                    } else {
                        // top item not yet due
                        break;
                    }
                } else {
                    // stale heap item
                    heap.pop();
                    continue;
                }
            } else {
                break;
            }
        }
        Ok(claimed)
    }

    async fn mark_attempt(&self, entry_id: &Uuid, success: bool, _error_reason: Option<String>) -> anyhow::Result<bool> {
        // If success -> remove entry if present; return true
        // If failure -> reschedule according to policy if attempts < max_retries; otherwise remove/expire
        let mut entries_guard = self.entries.write().await;
        if let Some(mut e) = entries_guard.remove(entry_id) {
            if success {
                metrics::increment_counter!("retry_queue_succeeded");
                // removed permanently
                return Ok(true);
            } else {
                let attempts = e.attempts + 1;
                e.attempts = attempts;
                e.last_attempt_at = Some(Utc::now());

                // check expiration
                if let Some(exp) = e.expires_at {
                    if exp <= Utc::now() {
                        metrics::increment_counter!("retry_queue_expired");
                        return Ok(true); // considered removed/expired
                    }
                }

                if attempts > e.policy.max_retries() {
                    // discard or move to permanent DLQ (not implemented)
                    metrics::increment_counter!("retry_queue_max_retries_exceeded");
                    return Ok(true);
                }

                // compute next attempt
                let next_secs = e.policy.next_interval_seconds(attempts - 1); // attempt_count 0-based for interval calc
                e.next_attempt_at = Utc::now() + chrono::Duration::seconds(next_secs as i64);

                // reinsert
                let tenant = e.tenant_id.clone();
                let id = e.id;
                entries_guard.insert(id, e.clone());
                drop(entries_guard);

                self.push_heap(&tenant, HeapItem {
                    next_attempt_at: e.next_attempt_at,
                    priority: e.priority,
                    id,
                }).await;

                metrics::increment_counter!("retry_queue_rescheduled");
                return Ok(false);
            }
        } else {
            // not found => probably already removed
            Ok(true)
        }
    }

    async fn remove(&self, entry_id: &Uuid) -> anyhow::Result<()> {
        let mut entries = self.entries.write().await;
        if let Some(e) = entries.remove(entry_id) {
            // push a stale marker removal in heap will be handled lazily
            metrics::increment_counter!("retry_queue_removed");
            Ok(())
        } else {
            Ok(())
        }
    }

    async fn purge_expired(&self) -> anyhow::Result<usize> {
        let mut removed = 0usize;
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        let ids: Vec<Uuid> = entries.iter()
            .filter(|(_, e)| {
                if let Some(exp) = e.expires_at {
                    exp <= now
                } else {
                    false
                }
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            entries.remove(&id);
            removed += 1;
        }
        metrics::increment_counter!("retry_queue_purged_expired");
        Ok(removed)
    }

    async fn stats(&self, tenant_id: &str) -> anyhow::Result<RetryQueueStats> {
        let entries = self.entries.read().await;
        let total = entries.values().filter(|e| e.tenant_id == tenant_id).count();
        let now = Utc::now();
        let ready = entries.values().filter(|e| e.tenant_id == tenant_id && e.next_attempt_at <= now).count();
        Ok(RetryQueueStats {
            tenant_id: tenant_id.to_string(),
            queue_length: total,
            ready_count: ready,
        })
    }
}

#[cfg(feature = "postgres")]
pub mod postgres {
    //! Postgres-backed retry queue (outline).
    //!
    //! For production you'd implement durable persistence here using a table with indexes on tenant_id,
    //! next_attempt_at, and expires_at. Use transactions to atomically claim ready rows (SELECT ... FOR UPDATE SKIP LOCKED).
    //!
    //! This module is left as an outline; implementors should use `sqlx` or similar and follow best practices:
    //! - `claim_ready` using SKIP LOCKED to allow horizontal scaling
    //! - store the serialized payload as JSONB
    //! - maintain attempts counter, last_attempt_at, next_attempt_at, expires_at
    //! - expose efficient queries for purge and stats.
    use super::*;
    // TODO: implement PostgresRetryQueue using sqlx::PgPool
}

/// --------------------
/// Tests
/// --------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_enqueue_and_pop_and_mark_success() {
        let q = InMemoryRetryQueue::new();
        let id = Uuid::new_v4();
        let now = Utc::now();
        let entry = RetryEntry {
            id,
            tenant_id: "t1".into(),
            alert_id: Uuid::new_v4(),
            payload: json!({"x":1}),
            priority: Priority::High,
            attempts: 0,
            first_failed_at: now,
            last_attempt_at: None,
            next_attempt_at: now, // due immediately
            policy: RetryPolicy::Fixed { interval_seconds: 30, max_retries: 3 },
            expires_at: None,
        };

        q.enqueue(entry.clone()).await.unwrap();
        let stats = q.stats("t1").await.unwrap();
        assert_eq!(stats.queue_length, 1);

        let ready = q.peek_ready("t1", 10).await.unwrap();
        assert_eq!(ready.len(), 1);

        let claimed = q.pop_ready("t1", 10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, id);

        // mark success -> removed
        let removed = q.mark_attempt(&id, true, None).await.unwrap();
        assert!(removed);

        let stats2 = q.stats("t1").await.unwrap();
        assert_eq!(stats2.queue_length, 0);
    }

    #[tokio::test]
    async fn test_mark_attempt_reschedule_and_expire() {
        let q = InMemoryRetryQueue::new();
        let id = Uuid::new_v4();
        let now = Utc::now();
        let entry = RetryEntry {
            id,
            tenant_id: "t1".into(),
            alert_id: Uuid::new_v4(),
            payload: json!({"x":1}),
            priority: Priority::Medium,
            attempts: 0,
            first_failed_at: now,
            last_attempt_at: None,
            next_attempt_at: now,
            policy: RetryPolicy::Exponential { initial_seconds: 1, multiplier: 2.0, max_interval_seconds: 60, max_retries: 2 },
            expires_at: Some(now + chrono::Duration::seconds(1)), // very short expiry
        };

        q.enqueue(entry.clone()).await.unwrap();
        let claimed = q.pop_ready("t1", 10).await.unwrap();
        assert_eq!(claimed.len(), 1);

        // first failure -> rescheduled
        let removed = q.mark_attempt(&id, false, Some("net".into())).await.unwrap();
        assert!(!removed);

        // wait until expiry passes
        sleep(Duration::from_millis(1100)).await;

        // Now pop ready (if any) and mark attempt again -> should be expired and removed
        let claimed2 = q.pop_ready("t1", 10).await.unwrap();
        // claimed2 may be empty if next_attempt in future; call purge
        let purged = q.purge_expired().await.unwrap();
        assert!(purged >= 1);
    }
}
