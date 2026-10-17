//! リモート向けのライブ再生
//!
//! オーディオ入力のコールバックから全チャンネル分のサンプルを 1 まとめ(`LiveFrame`)で受け取り、
//! 聞いている人がいるチャンネルの組み合わせ(ミックス)ごとに合成 → AAC (ADTS) にエンコードして配信する。
//!
//! - 同じ組み合わせを聞く人同士はエンコーダを共有する(`broadcast` で配る)
//! - 聞く人がいなくなったミックスは次のフレームで捨てる
//! - どのミックスも無いときは `active` フラグを落とし、入力側はフレームを作らない
//! - 同じコールバック由来のサンプルを足すので、チャンネル間の時間ずれは起きない

use crate::aac::{AacEncoder, AacProfile};
use anyhow::{bail, Result};
use axum::body::Bytes;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

/// 1 ミックスあたりの配信バッファ(1 要素 ≒ AAC 1 フレーム ≒ 21 ms)。
/// これ以上遅れたクライアントは古いフレームを飛ばして追いつかせる。
const LIVE_BROADCAST_CAPACITY: usize = 256;

/// 1 回のオーディオコールバック分のサンプル(チャンネル ID とモノラルサンプル)
#[derive(Debug, Clone)]
pub struct LiveFrame {
    pub channels: Vec<(usize, Vec<i16>)>,
}

struct Mix {
    encoder: AacEncoder,
    tx: broadcast::Sender<Bytes>,
    buf: Vec<u8>,
}

pub struct LiveHub {
    sample_rate: u32,
    bitrate: u32,
    profile: AacProfile,
    /// 配信してよいチャンネル(有効なチャンネル)
    channels: Vec<usize>,
    /// キーは昇順・重複なしのチャンネル ID 列
    mixes: Mutex<HashMap<Vec<usize>, Mix>>,
    active: Arc<AtomicBool>,
}

impl LiveHub {
    pub fn new(sample_rate: u32, bitrate: u32, profile: AacProfile, channels: Vec<usize>) -> Arc<Self> {
        Arc::new(Self {
            sample_rate,
            bitrate,
            profile,
            channels,
            mixes: Mutex::new(HashMap::new()),
            active: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 聞いている人がいるか(入力側がフレームを作るかどうかの判定に使う)
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        self.active.clone()
    }

    /// 指定チャンネルのミックスを購読する。無ければ作る
    pub fn subscribe(&self, channels: &[usize]) -> Result<broadcast::Receiver<Bytes>> {
        let mut key = channels.to_vec();
        key.sort_unstable();
        key.dedup();
        if key.is_empty() {
            bail!("チャンネルが指定されていません");
        }
        if let Some(c) = key.iter().find(|c| !self.channels.contains(c)) {
            bail!("チャンネル {} は配信できません", c);
        }

        let mut mixes = self.mixes.lock().unwrap();
        if let Some(mix) = mixes.get(&key) {
            return Ok(mix.tx.subscribe());
        }
        let (tx, rx) = broadcast::channel(LIVE_BROADCAST_CAPACITY);
        log::info!("ライブ配信: ミックス開始 {:?}", key);
        mixes.insert(
            key,
            Mix {
                encoder: AacEncoder::new(self.sample_rate, self.bitrate, self.profile)?,
                tx,
                buf: Vec::new(),
            },
        );
        self.active.store(true, Ordering::Relaxed);
        Ok(rx)
    }

    /// 1 フレーム分を各ミックスに流す
    pub fn process(&self, frame: &LiveFrame) {
        let mut mixes = self.mixes.lock().unwrap();
        mixes.retain(|key, mix| {
            if mix.tx.receiver_count() == 0 {
                log::info!("ライブ配信: ミックス終了 {:?}", key);
                return false;
            }
            mix.buf.clear();
            if let Err(e) = mix.encoder.encode(&mix_channels(frame, key), &mut mix.buf) {
                log::error!("ライブ配信: エンコード失敗のためミックス {:?} を終了: {}", key, e);
                return false;
            }
            if !mix.buf.is_empty() {
                let _ = mix.tx.send(Bytes::copy_from_slice(&mix.buf));
            }
            true
        });
        self.active.store(!mixes.is_empty(), Ordering::Relaxed);
    }

    /// 入力側から届くフレームを処理し続ける
    pub async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<LiveFrame>) {
        while let Some(frame) = rx.recv().await {
            self.process(&frame);
        }
    }
}

/// 選択チャンネルを足し合わせる(クリップは飽和)。フレームに無いチャンネルは無音扱い
fn mix_channels(frame: &LiveFrame, channels: &[usize]) -> Vec<i16> {
    let len = frame.channels.iter().map(|(_, s)| s.len()).max().unwrap_or(0);
    let mut acc = vec![0i32; len];
    for (ch, samples) in &frame.channels {
        if channels.contains(ch) {
            for (a, &s) in acc.iter_mut().zip(samples) {
                *a += s as i32;
            }
        }
    }
    acc.into_iter()
        .map(|a| a.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, values: &[(usize, i16)]) -> LiveFrame {
        LiveFrame {
            channels: values.iter().map(|&(c, v)| (c, vec![v; len])).collect(),
        }
    }

    #[test]
    fn mix_sums_selected_channels_only() {
        let f = frame(4, &[(2, 100), (3, 20), (4, 7)]);
        assert_eq!(mix_channels(&f, &[2, 4]), vec![107; 4]);
        assert_eq!(mix_channels(&f, &[3]), vec![20; 4]);
        // フレームに無いチャンネルは無音
        assert_eq!(mix_channels(&f, &[9]), vec![0; 4]);
    }

    #[test]
    fn mix_saturates() {
        let f = frame(2, &[(0, 30000), (1, 30000)]);
        assert_eq!(mix_channels(&f, &[0, 1]), vec![i16::MAX; 2]);
        let f = frame(2, &[(0, -30000), (1, -30000)]);
        assert_eq!(mix_channels(&f, &[0, 1]), vec![i16::MIN; 2]);
    }

    #[test]
    fn subscribe_validates_channels() {
        let hub = LiveHub::new(48000, 48000, AacProfile::Lc, vec![2, 3]);
        assert!(hub.subscribe(&[]).is_err());
        assert!(hub.subscribe(&[1]).is_err());
        assert!(hub.subscribe(&[2, 9]).is_err());
        assert!(!hub.active.load(Ordering::Relaxed));
        assert!(hub.subscribe(&[3, 2, 2]).is_ok());
        assert!(hub.active.load(Ordering::Relaxed));
    }

    #[test]
    fn listeners_of_same_set_share_one_mix() {
        let hub = LiveHub::new(48000, 48000, AacProfile::Lc, vec![2, 3]);
        let _a = hub.subscribe(&[2, 3]).unwrap();
        let _b = hub.subscribe(&[3, 2]).unwrap();
        let _c = hub.subscribe(&[2]).unwrap();
        assert_eq!(hub.mixes.lock().unwrap().len(), 2);
    }

    #[test]
    fn delivers_adts_and_drops_mix_without_listeners() {
        let hub = LiveHub::new(48000, 48000, AacProfile::Lc, vec![0, 1]);
        let mut rx = hub.subscribe(&[0, 1]).unwrap();

        // 10 ms × 30 = 300 ms 分 → AAC フレーム(1024 サンプル)が十数個出る
        for _ in 0..30 {
            hub.process(&frame(480, &[(0, 1000), (1, -200)]));
        }
        let mut bytes = Vec::new();
        while let Ok(b) = rx.try_recv() {
            bytes.extend_from_slice(&b);
        }
        assert!(bytes.len() > 100);
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1] & 0xF0, 0xF0);
        assert!(!crate::aac::tests::decode(&bytes).is_empty());

        drop(rx);
        hub.process(&frame(480, &[(0, 0)]));
        assert!(hub.mixes.lock().unwrap().is_empty());
        assert!(!hub.active.load(Ordering::Relaxed));
    }
}
