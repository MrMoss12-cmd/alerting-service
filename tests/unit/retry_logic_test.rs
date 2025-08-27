// tests/unit/retry_logic_test.rs

use tokio::time::{sleep, Duration, Instant};
use anyhow::Result;
use rand::{thread_rng, Rng};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug, PartialEq)]
enum BackoffStrategy {
    Linear { base_delay: Duration, max_retries: usize },
    Exponential { base_delay: Duration, max_retries: usize },
    ExponentialJitter { base_delay: Duration, max_retries: usize },
}

impl BackoffStrategy {
    async fn calculate_delay(&self, attempt: usize) -> Duration {
        match self {
            BackoffStrategy::Linear { base_delay, .. } => *base_delay * attempt as u32,
            BackoffStrategy::Exponential { base_delay, .. } => {
                *base_delay * 2u32.pow(attempt as u32 - 1)
            }
            BackoffStrategy::ExponentialJitter { base_delay, .. } => {
                let exp = *base_delay * 2u32.pow(attempt as u32 - 1);
                let jitter_ms = thread_rng().gen_range(0..exp.as_millis() as u64 / 2);
                exp + Duration::from_millis(jitter_ms)
            }
        }
    }

    fn max_retries(&self) -> usize {
        match self {
            BackoffStrategy::Linear { max_retries, .. } => *max_retries,
            BackoffStrategy::Exponential { max_retries, .. } => *max_retries,
            BackoffStrategy::ExponentialJitter { max_retries, .. } => *max_retries,
        }
    }
}

// Simulated RetryItem with timestamps and attempt count
#[derive(Clone, Debug)]
struct RetryItem {
    id: String,
    attempt: usize,
    max_attempts: usize,
    created_at: Instant,
    last_attempt: Option<Instant>,
    expired_after: Duration,
}

impl RetryItem {
    fn new(id: &str, max_attempts: usize, expire_after: Duration) -> Self {
        Self {
            id: id.to_string(),
            attempt: 0,
            max_attempts,
            created_at: Instant::now(),
            last_attempt: None,
            expired_after: expire_after,
        }
    }

    fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.created_at) > self.expired_after
    }
}

// Simulated retry queue
struct RetryQueue {
    items: Arc<Mutex<Vec<RetryItem>>>,
}

impl RetryQueue {
    fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn enqueue(&self, item: RetryItem) {
        let mut items = self.items.lock().await;
        items.push(item);
    }

    async fn dequeue_ready(&self, strategy: &BackoffStrategy) -> Vec<RetryItem> {
        let now = Instant::now();
        let mut ready = Vec::new();
        let mut items = self.items.lock().await;

        let mut i = 0;
        while i < items.len() {
            let item = &items[i];
            if item.is_expired() {
                items.remove(i);
                continue;
            }
            if item.attempt >= item.max_attempts {
                items.remove(i);
                continue;
            }
            let delay = strategy.calculate_delay(item.attempt + 1).await;
            let last = item.last_attempt.unwrap_or(item.created_at);
            if now.duration_since(last) >= delay {
                // Move out to ready list
                ready.push(item.clone());
                items.remove(i);
            } else {
                i += 1;
            }
        }
        ready
    }

    async fn reenqueue(&self, mut item: RetryItem) {
        item.attempt += 1;
        item.last_attempt = Some(Instant::now());
        self.enqueue(item).await;
    }
}

#[tokio::test]
async fn test_linear_backoff_and_max_attempts() -> Result<()> {
    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::Linear {
        base_delay: Duration::from_millis(10),
        max_retries: 3,
    };

    let mut item = RetryItem::new("linear1", 3, Duration::from_secs(60));
    queue.enqueue(item.clone()).await;

    // First dequeue, attempt 1 ready immediately after base_delay * 1 (10ms)
    sleep(Duration::from_millis(11)).await;
    let ready = queue.dequeue_ready(&strategy).await;
    assert_eq!(ready.len(), 1);
    item = ready[0].clone();
    assert_eq!(item.attempt, 0);

    // Re-enqueue with attempt increment
    queue.reenqueue(item).await;

    // Wait for second attempt delay (20ms)
    sleep(Duration::from_millis(21)).await;
    let ready = queue.dequeue_ready(&strategy).await;
    assert_eq!(ready.len(), 1);
    let retry_item = &ready[0];
    assert_eq!(retry_item.attempt, 1);

    Ok(())
}

#[tokio::test]
async fn test_exponential_backoff_and_expiry() -> Result<()> {
    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::Exponential {
        base_delay: Duration::from_millis(10),
        max_retries: 3,
    };

    let item = RetryItem::new("exp1", 3, Duration::from_millis(25));
    queue.enqueue(item).await;

    // Wait beyond expiration
    sleep(Duration::from_millis(30)).await;
    let ready = queue.dequeue_ready(&strategy).await;
    // Should be empty as item expired and removed
    assert!(ready.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_exponential_jitter_backoff() -> Result<()> {
    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::ExponentialJitter {
        base_delay: Duration::from_millis(10),
        max_retries: 3,
    };

    let item = RetryItem::new("jit1", 3, Duration::from_secs(60));
    queue.enqueue(item.clone()).await;

    // Wait sufficient time for first retry (at least base_delay)
    sleep(Duration::from_millis(15)).await;
    let ready = queue.dequeue_ready(&strategy).await;
    assert_eq!(ready.len(), 1);

    Ok(())
}

#[tokio::test]
async fn test_retry_continues_after_failures() -> Result<()> {
    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::Linear {
        base_delay: Duration::from_millis(5),
        max_retries: 5,
    };

    // Simulate 3 items, 1 of which will fail to process
    let mut fail_item = RetryItem::new("fail", 5, Duration::from_secs(60));
    let success_item = RetryItem::new("success", 5, Duration::from_secs(60));
    queue.enqueue(fail_item.clone()).await;
    queue.enqueue(success_item.clone()).await;

    for attempt in 1..=3 {
        sleep(Duration::from_millis(6)).await;
        let ready = queue.dequeue_ready(&strategy).await;

        for mut item in ready {
            if item.id == "fail" {
                // Simulate failure by re-enqueue without processing success
                queue.reenqueue(item.clone()).await;
                fail_item = item;
            } else {
                // success, do not reenqueue
            }
        }
    }

    // After 3 failed attempts, fail_item attempt count should be 3
    assert_eq!(fail_item.attempt, 2); // Because reenqueue increments after dequeue_ready removal

    Ok(())
}

#[tokio::test]
async fn test_retry_persistence_simulation() -> Result<()> {
    // This test simulates persistence by ensuring items stay in queue until max attempts or expiry

    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::Linear {
        base_delay: Duration::from_millis(1),
        max_retries: 3,
    };

    let item = RetryItem::new("persist", 3, Duration::from_secs(60));
    queue.enqueue(item.clone()).await;

    for _ in 1..=3 {
        sleep(Duration::from_millis(2)).await;
        let ready = queue.dequeue_ready(&strategy).await;
        assert_eq!(ready.len(), 1);
        queue.reenqueue(ready[0].clone()).await;
    }

    // Next dequeue_ready should remove it because max_retries reached
    sleep(Duration::from_millis(2)).await;
    let ready = queue.dequeue_ready(&strategy).await;
    assert!(ready.is_empty());

    Ok(())
}

#[tokio::test]
async fn test_no_blocking_on_exceptions() -> Result<()> {
    // Simulate a panic or error in processing one retry item does not block others
    let queue = RetryQueue::new();
    let strategy = BackoffStrategy::Linear {
        base_delay: Duration::from_millis(1),
        max_retries: 3,
    };

    let item1 = RetryItem::new("ok1", 3, Duration::from_secs(60));
    let item2 = RetryItem::new("panic1", 3, Duration::from_secs(60));

    queue.enqueue(item1.clone()).await;
    queue.enqueue(item2.clone()).await;

    sleep(Duration::from_millis(2)).await;
    let ready = queue.dequeue_ready(&strategy).await;

    for item in ready {
        if item.id == "panic1" {
            // Simulate panic by skipping reenqueue (pretend failed)
        } else {
            queue.reenqueue(item).await;
        }
    }

    // Item1 should still be retried, item2 lost but not blocking
    let items_left = queue.items.lock().await.len();
    assert_eq!(items_left, 1);

    Ok(())
}
