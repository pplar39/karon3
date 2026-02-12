use crate::types::LatencyEvent;
use tokio::sync::mpsc;

pub const INTENT_QUEUE_CAPACITY: usize = 4096;

pub fn now_us() -> u64 {
    crate::time::monotonic_now_us()
}

pub fn mark_detection_enqueue(latency: &mut LatencyEvent) {
    if latency.ws_recv_ts_us == 0 {
        latency.ws_recv_ts_us = latency.ws_receive_us;
    }
    if latency.parse_done_ts_us == 0 {
        latency.parse_done_ts_us = latency.pool_parsed_us;
    }
    latency.intent_enqueued_ts_us = now_us();
    latency.detection_latency_us = latency
        .intent_enqueued_ts_us
        .saturating_sub(latency.ws_recv_ts_us);
}

pub fn try_enqueue_drop_newest<T>(tx: &mpsc::Sender<T>, item: T) -> bool {
    match tx.try_send(item) {
        Ok(_) => true,
        Err(mpsc::error::TrySendError::Full(_)) => false,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, Instant};

    struct MockJitoSender {
        fail_429_count: usize,
        backoff: Duration,
    }

    impl MockJitoSender {
        fn new(fail_429_count: usize, backoff: Duration) -> Self {
            Self {
                fail_429_count,
                backoff,
            }
        }

        async fn send_bundle_with_retry(&self) -> Duration {
            let mut backoff_total = Duration::ZERO;
            for _ in 0..self.fail_429_count {
                tokio::time::sleep(self.backoff).await;
                backoff_total += self.backoff;
            }
            backoff_total
        }
    }

    #[tokio::test]
    async fn detection_span_stays_small_when_execution_backoffs() {
        let (intent_tx, mut intent_rx) = mpsc::channel::<LatencyEvent>(32);
        let (done_tx, mut done_rx) = mpsc::channel::<LatencyEvent>(32);
        let sender = MockJitoSender::new(2, Duration::from_millis(20));

        tokio::spawn(async move {
            while let Some(mut latency) = intent_rx.recv().await {
                latency.exec_start_ts_us = latency.intent_enqueued_ts_us + 1;
                let backoff_start = Instant::now();
                let backoff_total = sender.send_bundle_with_retry().await;
                let backoff_end = backoff_start + backoff_total;
                latency.backoff_sleep_start_ts_us = latency.exec_start_ts_us + 1;
                latency.backoff_sleep_end_ts_us =
                    latency.backoff_sleep_start_ts_us + backoff_total.as_micros() as u64;
                latency.backoff_latency_us = latency
                    .backoff_sleep_end_ts_us
                    .saturating_sub(latency.backoff_sleep_start_ts_us);
                latency.execution_latency_us = (backoff_end - backoff_start).as_micros() as u64 + 2;
                let _ = done_tx.send(latency).await;
            }
        });

        for i in 0..8_u64 {
            let mut latency = LatencyEvent::default();
            latency.ws_recv_ts_us = 1_000_000 + (i * 1_000);
            latency.parse_done_ts_us = latency.ws_recv_ts_us + 20;
            latency.intent_enqueued_ts_us = latency.parse_done_ts_us + 30;
            latency.detection_latency_us = latency
                .intent_enqueued_ts_us
                .saturating_sub(latency.ws_recv_ts_us);
            assert!(try_enqueue_drop_newest(&intent_tx, latency));
        }
        drop(intent_tx);

        let mut max_detection = 0_u64;
        let mut max_execution = 0_u64;
        let mut max_backoff = 0_u64;

        for _ in 0..8 {
            let latency = tokio::time::timeout(Duration::from_secs(1), done_rx.recv())
                .await
                .expect("timed out waiting for done latency")
                .expect("done channel closed early");
            if latency.detection_latency_us > max_detection {
                max_detection = latency.detection_latency_us;
            }
            if latency.execution_latency_us > max_execution {
                max_execution = latency.execution_latency_us;
            }
            if latency.backoff_latency_us > max_backoff {
                max_backoff = latency.backoff_latency_us;
            }
        }

        assert!(max_detection < 5_000);
        assert!(max_execution >= 40_000);
        assert!(max_backoff >= 40_000);
    }
}
