//! 音声コールバックの停止とドライバエラーを、音量とは独立に監視する。
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const STALL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct StreamHealth(Arc<Inner>);

struct Inner {
    origin: Instant,
    enabled: AtomicBool,
    last_ms: AtomicU64,
    error_until_ms: AtomicU64,
}

impl Default for StreamHealth {
    fn default() -> Self {
        Self(Arc::new(Inner {
            origin: Instant::now(),
            enabled: AtomicBool::new(false),
            last_ms: AtomicU64::new(0),
            error_until_ms: AtomicU64::new(0),
        }))
    }
}

impl StreamHealth {
    pub fn start(&self) {
        self.0.error_until_ms.store(0, Ordering::Relaxed);
        self.heartbeat();
        self.0.enabled.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.0.enabled.store(false, Ordering::Relaxed);
    }

    // RT コールバックではロック・ログ出力・メモリ確保を行わない。
    pub fn heartbeat(&self) {
        self.heartbeat_at(self.0.origin.elapsed().as_millis() as u64);
    }

    fn heartbeat_at(&self, now_ms: u64) {
        self.0.last_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn report_error(&self) {
        // ハートビート側はこの値を消さない。エラー通知との競合で警告を失わないため。
        let now_ms = self.0.origin.elapsed().as_millis() as u64;
        self.0
            .error_until_ms
            .store(now_ms.saturating_add(3000), Ordering::Relaxed);
    }

    pub fn problem(&self) -> Option<&'static str> {
        self.problem_at(self.0.origin.elapsed())
    }

    fn problem_at(&self, elapsed: Duration) -> Option<&'static str> {
        if !self.0.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let last = self.0.last_ms.load(Ordering::Relaxed);
        if last < self.0.error_until_ms.load(Ordering::Relaxed) {
            return Some("デバイスエラー：接続を確認してください");
        }
        if elapsed.as_millis().saturating_sub(last as u128) >= STALL_TIMEOUT.as_millis() {
            return Some("音声ストリーム停止：3秒以上データが届いていません");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_devices_do_not_alert_even_after_twelve_hours() {
        let health = StreamHealth::default();
        assert!(health.problem_at(Duration::from_secs(43200)).is_none());
        health.start();
        health.report_error();
        health.stop();
        assert!(health.problem().is_none());
    }

    #[test]
    fn missing_callbacks_alert_and_resumption_clears() {
        let health = StreamHealth::default();
        health.start();
        let last = health.0.last_ms.load(Ordering::Relaxed);
        assert!(health
            .problem_at(Duration::from_millis(last + 2999))
            .is_none());
        assert!(health
            .problem_at(Duration::from_millis(last + 3000))
            .is_some());
        health.report_error();
        assert!(health.problem().unwrap().contains("デバイスエラー"));
        health.heartbeat();
        assert!(health.problem().is_some());
        let recovered_ms = health.0.error_until_ms.load(Ordering::Relaxed);
        health.heartbeat_at(recovered_ms);
        assert!(health
            .problem_at(Duration::from_millis(recovered_ms))
            .is_none());
    }

    #[test]
    fn continuous_silent_callbacks_remain_healthy_for_twelve_hours() {
        let health = StreamHealth::default();
        health.start();
        for second in 0..=43200 {
            health.0.last_ms.store(second * 1000, Ordering::Relaxed);
            assert!(health
                .problem_at(Duration::from_millis(second * 1000 + 200))
                .is_none());
        }
    }
}
