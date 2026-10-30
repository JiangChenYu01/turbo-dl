//! 每连接限速器 + 跨任务/跨连接全局限速器。

use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// 基于滑动时间窗的简单限速器：按已发送字节数推算应经过的时间，超限则休眠补齐。
pub struct RateLimiter {
    limit_bytes_per_sec: u64,
    start: Instant,
    sent: u64,
}

impl RateLimiter {
    pub fn new(limit_bytes_per_sec: u64) -> Self {
        Self {
            limit_bytes_per_sec: limit_bytes_per_sec.max(1),
            start: Instant::now(),
            sent: 0,
        }
    }

    /// 每写入一个数据块后调用，按需休眠以维持平均速率。
    pub async fn throttle(&mut self, bytes: u64) {
        self.sent += bytes;
        let expected_secs = self.sent as f64 / self.limit_bytes_per_sec as f64;
        let elapsed = self.start.elapsed().as_secs_f64();
        if expected_secs > elapsed {
            let gap = expected_secs - elapsed;
            if gap > 0.0 {
                sleep(Duration::from_secs_f64(gap.min(5.0))).await;
            }
        }
    }
}

/// 跨任务/跨连接共享的全局限速器：所有 worker 并发注册字节数，
/// 各自在锁外休眠，使整个下载器的总吞吐收敛到设定值。
#[derive(Debug)]
pub struct GlobalRateLimiter {
    limit_bytes_per_sec: u64,
    state: Mutex<(Instant, u64)>,
}

impl GlobalRateLimiter {
    pub fn new(limit_bytes_per_sec: u64) -> Self {
        Self {
            limit_bytes_per_sec: limit_bytes_per_sec.max(1),
            state: Mutex::new((Instant::now(), 0)),
        }
    }

    /// 注册 bytes 字节，返回调用方需要等待的时长（在锁外 sleep）。
    pub fn register(&self, bytes: u64) -> Duration {
        let mut st = self.state.lock().unwrap();
        st.1 += bytes;
        let expected_secs = st.1 as f64 / self.limit_bytes_per_sec as f64;
        let elapsed = st.0.elapsed().as_secs_f64();
        if expected_secs > elapsed {
            let gap = expected_secs - elapsed;
            if gap > 0.0 {
                return Duration::from_secs_f64(gap.min(5.0));
            }
        }
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rate_limiter_throttles_to_limit() {
        let mut l = RateLimiter::new(1000); // 1 KiB/s
        let start = Instant::now();
        l.throttle(1000).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(900),
            "限速器应至少休眠 ~1s，实际 {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn rate_limiter_allows_small_burst() {
        let mut l = RateLimiter::new(1000);
        let start = Instant::now();
        l.throttle(100).await; // 远小于限速，不应长眠
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn global_limiter_accumulates_delay() {
        let g = GlobalRateLimiter::new(1000);
        // 第一次注册少量字节，第二次注册后应产生等待时间
        let _ = g.register(100);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let d = g.register(900);
        assert!(d > Duration::ZERO, "累计字节应触发等待，实际 {:?}", d);
    }
}