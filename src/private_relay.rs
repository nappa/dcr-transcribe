//! iCloud プライベートリレー経由のアクセスを止める
//!
//! プライベートリレーは閲覧者の IP を隠し、出口の IP は Apple が持つ。Cloudflare の無料プランには
//! 「プライベートリレーか」を判定する項目が無く、自分で作れる IP リストも 1 万件までで Apple の一覧
//! (約 28 万件)が入らない。そこで Apple が公開している出口 IP の一覧を定期的に取ってきて、
//! `CF-Connecting-IP` と照らし合わせる。
//!
//! - 一覧: <https://mask-api.icloud.com/egress-ip-ranges.csv>(`prefix,国,地域,都市,` の CSV、約 12MB)
//! - 範囲は開始位置で並べて重なりをまとめ、二分探索で引く(IPv4 は u32、IPv6 は u128)
//! - 起動時は手元の写し(`private_relay_cache`)を読み、1 日ごとに取り直して写しも更新する
//! - 取り直しに失敗したら古い一覧のまま続ける。一度も読めていないうちは止めない(全員を締め出さないため)

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

/// Apple が公開しているプライベートリレーの出口 IP の一覧
pub const APPLE_EGRESS_URL: &str = "https://mask-api.icloud.com/egress-ip-ranges.csv";
/// 取り直す間隔
const REFRESH: Duration = Duration::from_secs(24 * 3600);
/// 失敗したときにやり直すまでの間隔
const RETRY: Duration = Duration::from_secs(15 * 60);
/// これより少ない一覧は壊れているとみなして使わない
const MIN_ENTRIES: usize = 10_000;
/// 取ってくる大きさの上限
const MAX_BYTES: usize = 64 << 20;

/// 出口の IP 範囲(開始, 終了 両端含む)
#[derive(Debug, Default)]
pub struct EgressList {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
    entries: usize,
}

impl EgressList {
    /// Apple の CSV を読む。読めない行は飛ばす
    pub fn parse(csv: &str) -> Self {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        let mut entries = 0;
        for line in csv.lines() {
            let prefix = line.split(',').next().unwrap_or("").trim();
            let Some((addr, len)) = prefix.split_once('/') else { continue };
            let Ok(len) = len.parse::<u32>() else { continue };
            match addr.parse::<IpAddr>() {
                Ok(IpAddr::V4(a)) if len <= 32 => {
                    let start = u32::from(a) & mask32(len);
                    v4.push((start, start | !mask32(len)));
                    entries += 1;
                }
                Ok(IpAddr::V6(a)) if len <= 128 => {
                    let start = u128::from(a) & mask128(len);
                    v6.push((start, start | !mask128(len)));
                    entries += 1;
                }
                _ => {}
            }
        }
        Self { v4: merge(v4), v6: merge(v6), entries }
    }

    pub fn entries(&self) -> usize {
        self.entries
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(a) => within(&self.v4, u32::from(a)),
            // IPv4 を IPv6 で表したもの(::ffff:a.b.c.d)も IPv4 として引く
            IpAddr::V6(a) => match a.to_ipv4_mapped() {
                Some(v4) => within(&self.v4, u32::from(v4)),
                None => within(&self.v6, u128::from(a)),
            },
        }
    }
}

fn mask32(len: u32) -> u32 {
    if len == 0 { 0 } else { u32::MAX << (32 - len) }
}

fn mask128(len: u32) -> u128 {
    if len == 0 { 0 } else { u128::MAX << (128 - len) }
}

/// 開始位置で並べ、重なる・隣り合う範囲をまとめる
fn merge<T: Ord + Copy + num_like::Succ>(mut v: Vec<(T, T)>) -> Vec<(T, T)> {
    v.sort_unstable();
    let mut out: Vec<(T, T)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        if let Some(last) = out.last_mut() {
            if s <= last.1.succ() {
                if e > last.1 {
                    last.1 = e;
                }
                continue;
            }
        }
        out.push((s, e));
    }
    out.shrink_to_fit();
    out
}

fn within<T: Ord + Copy>(ranges: &[(T, T)], x: T) -> bool {
    // x 以下で始まる最後の範囲に入っているか
    let i = ranges.partition_point(|(s, _)| *s <= x);
    i > 0 && ranges[i - 1].1 >= x
}

mod num_like {
    /// 次の値(最大値ならそのまま)
    pub trait Succ {
        fn succ(self) -> Self;
    }
    impl Succ for u32 {
        fn succ(self) -> Self {
            self.saturating_add(1)
        }
    }
    impl Succ for u128 {
        fn succ(self) -> Self {
            self.saturating_add(1)
        }
    }
}

/// 閲覧サーバが持つ判定器。一覧は裏で取り直す
pub struct PrivateRelayFilter {
    list: RwLock<Option<Arc<EgressList>>>,
    url: String,
    cache: PathBuf,
    /// 一覧が無いまま来たことを一度だけ警告する
    warned_empty: AtomicBool,
}

impl PrivateRelayFilter {
    pub fn new(url: &str, cache: &str) -> Arc<Self> {
        Arc::new(Self {
            list: RwLock::new(None),
            url: url.to_string(),
            cache: PathBuf::from(cache),
            warned_empty: AtomicBool::new(false),
        })
    }

    /// 要求をプライベートリレー経由として止めるか(Cloudflare を通った要求の `CF-Connecting-IP` で判定)
    pub fn blocks(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        else {
            return false;
        };
        let list = self.list.read().ok().and_then(|l| l.clone());
        match list {
            Some(list) => list.contains(ip),
            None => {
                if !self.warned_empty.swap(true, Ordering::Relaxed) {
                    log::warn!("プライベートリレー: 一覧をまだ読めていないので、止めずに通しています");
                }
                false
            }
        }
    }

    pub(crate) fn set(&self, list: EgressList, from: &str) {
        log::info!(
            "プライベートリレー: 出口 IP {} 件を読みました({}、範囲 IPv4 {} / IPv6 {})",
            list.entries,
            from,
            list.v4.len(),
            list.v6.len()
        );
        if let Ok(mut cur) = self.list.write() {
            *cur = Some(Arc::new(list));
        }
    }

    /// 手元の写しを読み、その後は定期的に取り直す。閲覧サーバが無くなったら終わる
    pub async fn run(self: Arc<Self>) {
        let weak = Arc::downgrade(&self);
        let fresh = match tokio::fs::read_to_string(&self.cache).await {
            Ok(csv) => {
                let list = EgressList::parse(&csv);
                let age = std::fs::metadata(&self.cache).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                if list.entries >= MIN_ENTRIES {
                    self.set(list, "手元の写し");
                }
                age.is_some_and(|a| a < REFRESH)
            }
            Err(_) => false,
        };
        drop(self);
        let mut wait = if fresh { REFRESH } else { Duration::ZERO };
        loop {
            tokio::time::sleep(wait).await;
            let Some(me) = Weak::upgrade(&weak) else { break };
            wait = match me.refresh().await {
                Ok(()) => REFRESH,
                Err(e) => {
                    log::warn!("プライベートリレー: 一覧を取り直せません ({:#})。{} 分後にやり直します", e, RETRY.as_secs() / 60);
                    RETRY
                }
            };
        }
    }

    async fn refresh(&self) -> Result<()> {
        let csv = fetch(&self.url).await?;
        let list = EgressList::parse(&csv);
        if list.entries < MIN_ENTRIES {
            bail!("一覧が少なすぎます ({} 件)", list.entries);
        }
        // 写しは書いてから置き換える(途中で落ちても壊れた写しを残さない)
        if let Some(dir) = self.cache.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let tmp = self.cache.with_extension("tmp");
        match tokio::fs::write(&tmp, csv.as_bytes()).await.and(tokio::fs::rename(&tmp, &self.cache).await) {
            Ok(()) => {}
            Err(e) => log::warn!("プライベートリレー: 写しを保存できません {:?}: {}", self.cache, e),
        }
        self.set(list, "Apple から取得");
        Ok(())
    }
}

async fn fetch(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("HTTP クライアントを作れません")?;
    let resp = client.get(url).send().await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        bail!("一覧が大きすぎます");
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
        if body.len() > MAX_BYTES {
            bail!("一覧が大きすぎます");
        }
    }
    String::from_utf8(body).context("一覧が UTF-8 ではありません")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "172.224.226.0/27,GB,GB-EN,London,\n\
        172.224.226.32/31,GB,GB-SC,Aberdeen,\n\
        172.224.226.34/31,GB,GB-EN,Oxford,\n\
        104.28.0.0/24,JP,JP-13,Tokyo,\n\
        2a02:26f7:b3c0:4000::/64,JP,JP-13,Tokyo,\n\
        not-an-ip,JP,,,\n\
        10.0.0.0/33,JP,,,\n";

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_looks_up_v4_and_v6() {
        let l = EgressList::parse(SAMPLE);
        assert_eq!(l.entries(), 5, "壊れた行は飛ばす");
        // 隣り合う /27 と /31 2 つは 1 つの範囲にまとまる
        assert_eq!(l.v4.len(), 2);
        for hit in ["172.224.226.0", "172.224.226.35", "104.28.0.255", "2a02:26f7:b3c0:4000::1", "::ffff:104.28.0.7"] {
            assert!(l.contains(ip(hit)), "{}", hit);
        }
        for miss in ["172.224.226.36", "104.28.1.0", "8.8.8.8", "2a02:26f7:b3c0:4001::1", "::1"] {
            assert!(!l.contains(ip(miss)), "{}", miss);
        }
    }

    #[test]
    fn filter_uses_cf_connecting_ip_and_lets_through_until_loaded() {
        let f = PrivateRelayFilter::new("http://127.0.0.1:1/none", "/nonexistent/cache.csv");
        let mut h = axum::http::HeaderMap::new();
        h.insert("cf-connecting-ip", "104.28.0.9".parse().unwrap());
        assert!(!f.blocks(&h), "一覧が無いうちは止めない");
        f.set(EgressList::parse(SAMPLE), "テスト");
        assert!(f.blocks(&h));
        h.insert("cf-connecting-ip", "203.0.113.9".parse().unwrap());
        assert!(!f.blocks(&h));
        assert!(!f.blocks(&axum::http::HeaderMap::new()), "Cloudflare を通らない要求は判定しない");
    }

    /// Apple の実際の一覧を読む。`cargo test -- --ignored real_apple_egress_list`
    #[tokio::test]
    #[ignore]
    async fn real_apple_egress_list() {
        let csv = fetch(APPLE_EGRESS_URL).await.unwrap();
        let l = EgressList::parse(&csv);
        println!("entries={} v4 ranges={} v6 ranges={}", l.entries(), l.v4.len(), l.v6.len());
        assert!(l.entries() > MIN_ENTRIES);
        let first = csv.lines().next().unwrap().split(',').next().unwrap().split('/').next().unwrap();
        assert!(l.contains(ip(first)));
    }
}
