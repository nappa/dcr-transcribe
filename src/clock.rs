//! 時計のずれの監視(SNTP)
//!
//! 閲覧画面の時刻は「会場の時計で測った経過」を「中継サーバの時計」に載せて出すので、
//! どちらかの時計がずれていると表示の時刻もずれる。NTP サーバ(既定は time.cloudflare.com)に
//! 定期的に問い合わせて、この機械の時計のずれを測る。時計は直さない(直すのは chrony / OS の役目)。
//!
//! - NTP サーバとの差 = ((T2 − T1) + (T3 − T4)) / 2、往復 = (T4 − T1) − (T3 − T2)(RFC 4330)。
//!   記録するのは符号を反転した「この機械の時計 − 正しい時刻」(正なら進んでいる)
//! - 応答の origin timestamp が送った T1 と一致するものだけ受け付ける(なりすまし・取り違え防止)
//! - `warn_ms` を超えたらログに警告。閲覧画面・TUI にも出す

use crate::config::ClockConfig;
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 1900-01-01 から 1970-01-01 までの秒数
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// 時計のずれの警告の既定値(ms)。`[clock] warn_ms` を読めないところ(会場の TUI の中継表示)でも使う
pub const DEFAULT_WARN_MS: i64 = 500;
/// 往復がこれより長い測定は誤差が大きいので捨てる
const MAX_DELAY_MS: i64 = 2_000;

/// この機械の時計のずれ(この機械の時計 − 正しい時刻、ミリ秒。正なら進んでいる)
#[derive(Default)]
pub struct ClockMonitor {
    offset_ms: AtomicI64,
    delay_ms: AtomicI64,
    valid: AtomicBool,
    warn_ms: AtomicI64,
}

impl ClockMonitor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { warn_ms: AtomicI64::new(DEFAULT_WARN_MS), ..Default::default() })
    }

    /// 最後に測れたずれ。まだ測れていなければ None
    pub fn offset_ms(&self) -> Option<i64> {
        self.valid.load(Ordering::Relaxed).then(|| self.offset_ms.load(Ordering::Relaxed))
    }

    /// 最後の測定の往復時間(ずれの誤差はこの半分以内)
    pub fn delay_ms(&self) -> Option<i64> {
        self.valid.load(Ordering::Relaxed).then(|| self.delay_ms.load(Ordering::Relaxed))
    }

    /// 警告を出すずれ
    pub fn warn_ms(&self) -> i64 {
        self.warn_ms.load(Ordering::Relaxed)
    }

    fn record(&self, offset_ms: i64, delay_ms: i64) {
        self.offset_ms.store(offset_ms, Ordering::Relaxed);
        self.delay_ms.store(delay_ms, Ordering::Relaxed);
        self.valid.store(true, Ordering::Relaxed);
    }

    /// `running` が落ちるまで定期的に測る(設定で無効なら何もしない)
    pub async fn run(self: Arc<Self>, config: ClockConfig, running: Arc<AtomicBool>) {
        if !config.enabled {
            return;
        }
        self.warn_ms.store(config.warn_ms, Ordering::Relaxed);
        let interval = Duration::from_secs(config.interval_secs.max(16));
        let mut warned = false;
        let mut failures = 0u32;
        while running.load(Ordering::SeqCst) {
            match query(&config.server).await {
                Ok((server_minus_local, delay)) if delay <= MAX_DELAY_MS => {
                    let offset = -server_minus_local;
                    let first = self.offset_ms().is_none();
                    self.record(offset, delay);
                    failures = 0;
                    let bad = offset.abs() > config.warn_ms;
                    if bad && !warned {
                        log::warn!(
                            "時計: この機械の時計が {:+} ms ずれています({} と比較、往復 {} ms)。NTP の同期を確認してください",
                            offset, config.server, delay
                        );
                    } else if !bad && (warned || first) {
                        log::info!("時計: ずれ {:+} ms(往復 {} ms、{})", offset, delay, config.server);
                    }
                    warned = bad;
                }
                Ok((_, delay)) => log::debug!("時計: 往復 {} ms の測定を捨てました", delay),
                Err(e) => {
                    failures += 1;
                    // 一時的な失敗はよくあるので、続いたときだけ知らせる
                    if failures == 3 {
                        log::warn!("時計: {} に問い合わせできません ({:#})", config.server, e);
                    }
                }
            }
            // 止めるときに長く待たせないよう、細かく区切って眠る
            let until = tokio::time::Instant::now() + interval;
            while running.load(Ordering::SeqCst) && tokio::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// SNTP で 1 回問い合わせ、(サーバの時計 − この機械の時計, 往復) をミリ秒で返す
pub async fn query(server: &str) -> Result<(i64, i64)> {
    let addr = if server.contains(':') { server.to_string() } else { format!("{}:123", server) };
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.context("UDP ソケットを開けません")?;
    sock.connect(&addr).await.with_context(|| format!("{} に接続できません", addr))?;
    let t1 = now_ntp();
    sock.send(&request(t1)).await?;
    let mut buf = [0u8; 128];
    let n = tokio::time::timeout(QUERY_TIMEOUT, sock.recv(&mut buf))
        .await
        .context("応答がありません")??;
    let t4 = now_ntp();
    let (t2, t3) = parse_response(&buf[..n], t1)?;
    Ok(offset_delay(t1, t2, t3, t4))
}

fn request(t1: u64) -> [u8; 48] {
    let mut p = [0u8; 48];
    // LI=0, VN=4, Mode=3 (client)
    p[0] = 0x23;
    p[40..48].copy_from_slice(&t1.to_be_bytes());
    p
}

/// 応答から (T2 受信, T3 送信) を取り出す
fn parse_response(b: &[u8], t1: u64) -> Result<(u64, u64)> {
    if b.len() < 48 {
        bail!("応答が短すぎます ({} bytes)", b.len());
    }
    let li = b[0] >> 6;
    let mode = b[0] & 0x07;
    let stratum = b[1];
    if mode != 4 {
        bail!("サーバの応答ではありません (mode={})", mode);
    }
    if li == 3 || stratum == 0 || stratum > 15 {
        bail!("サーバが同期していません (LI={}, stratum={})", li, stratum);
    }
    let ts = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
    if ts(24) != t1 {
        bail!("こちらの問い合わせへの応答ではありません");
    }
    Ok((ts(32), ts(40)))
}

/// NTP の 64bit 時刻(上位 32bit が秒、下位 32bit が秒の小数)
fn now_ntp() -> u64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() + NTP_UNIX_OFFSET;
    let frac = ((d.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// NTP 時刻の差をミリ秒で(a − b)
fn diff_ms(a: u64, b: u64) -> i64 {
    let d = a.wrapping_sub(b) as i64;
    // 2^32 = 1 秒
    ((d as i128 * 1000) >> 32) as i64
}

fn offset_delay(t1: u64, t2: u64, t3: u64, t4: u64) -> (i64, i64) {
    let offset = (diff_ms(t2, t1) + diff_ms(t3, t4)) / 2;
    let delay = diff_ms(t4, t1) - diff_ms(t3, t2);
    (offset, delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1 << 32;

    #[test]
    fn offset_and_delay_from_four_timestamps() {
        let t1 = 1000 * SEC;
        // サーバの時計が 1.5 秒進んでいて、片道 50 ms、サーバ内 10 ms
        let t2 = t1 + SEC / 20 + SEC * 3 / 2;
        let t3 = t2 + SEC / 100;
        let t4 = t1 + SEC / 10 + SEC / 100;
        let (offset, delay) = offset_delay(t1, t2, t3, t4);
        // こちらは 1.5 秒遅れている = サーバ − こちら が +1500
        assert!((offset - 1500).abs() <= 1, "{}", offset);
        assert!((delay - 100).abs() <= 1, "{}", delay);
    }

    fn response(t1: u64, mode: u8, stratum: u8) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        b[0] = 0x20 | mode;
        b[1] = stratum;
        b[24..32].copy_from_slice(&t1.to_be_bytes());
        b[32..40].copy_from_slice(&(t1 + 1).to_be_bytes());
        b[40..48].copy_from_slice(&(t1 + 2).to_be_bytes());
        b
    }

    #[test]
    fn response_is_validated() {
        let t1 = now_ntp();
        assert_eq!(parse_response(&response(t1, 4, 2), t1).unwrap(), (t1 + 1, t1 + 2));
        assert!(parse_response(&response(t1, 3, 2), t1).is_err(), "mode が違う");
        assert!(parse_response(&response(t1, 4, 0), t1).is_err(), "kiss-o'-death / 未同期");
        assert!(parse_response(&response(t1 + 5, 4, 2), t1 + 6).is_err(), "別の問い合わせへの応答");
        assert!(parse_response(&response(t1, 4, 2)[..40], t1).is_err());
    }

    #[test]
    fn request_carries_transmit_timestamp() {
        let p = request(0x0102_0304_0506_0708);
        assert_eq!(p[0], 0x23);
        assert_eq!(&p[40..48], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// 実際の NTP サーバに問い合わせる。`cargo test -- --ignored real_ntp_query`
    #[tokio::test]
    #[ignore]
    async fn real_ntp_query() {
        let (offset, delay) = query("time.cloudflare.com").await.unwrap();
        println!("offset={} ms delay={} ms", offset, delay);
        assert!(delay < 2000);
        assert!(offset.abs() < 5000);
    }
}
