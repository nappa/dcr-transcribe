//! 中継サーバへのアップリンク(会場側)
//!
//! 会場の上り回線は細く不安定なので、閲覧者(数十人)に直接配信せず、東京などに置いた中継サーバ
//! (`crate::relay`)へ QUIC(UDP)1 本で送り、配信は中継サーバに任せる。cloudflared は通さず、
//! 私設 CA の証明書で互いを確かめる(mTLS、`crate::pki`)。
//!
//! ```text
//! [engine] ── push_transcript / push_segment / push_status ──> [Outbox]
//!     │                                                          │
//! [AudioInput] ─ LiveFrame ─> [Opus エンコード(聞かれている ch だけ)]  │
//!                                       │                        │
//!                                       └──────> [session: QUIC 1 本] ══> 中継サーバ
//!   制御・文字起こし・状態 ─ 双方向ストリーム 1 本(優先度 高、順序どおり)
//!   区間録音             ─ 1 件ごとに単方向ストリーム(優先度 低)
//!   ライブ音声           ─ データグラム(再送しない。欠けは中継サーバで Opus の FEC / 補間で埋める)
//! ```
//!
//! - 文字起こしには会場側の連番 `q`、区間録音には連番 `k` を振る。接続のたびに中継サーバが
//!   受け取り済みの `q` / `k` を返すので、その続きから送り直す(重複も欠損もしない)
//! - パケットが落ちても、待たされるのは落ちたストリームだけ(TCP のように全部が止まらない)
//! - ライブ音声は中継サーバで聞いている人がいるチャンネルだけ、Opus で 20 ms ごとに送る
//! - 4G の NAT は UDP を早く忘れるので 5 秒ごとに keep-alive。会場の IP が変わっても接続は続く(移行)

use crate::config::{ChannelConfig, UplinkConfig};
use crate::live::LiveFrame;
use crate::remote_server::{status_changed, ChannelInfo, ChannelStatus, WireEvent, WireStatus};
use crate::segment_recorder::SegmentInfo;
use crate::tui_state::Health;
use crate::types::TranscriptResult;
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, watch, Notify};

/// 手元に残す区間録音の数(中継サーバが再起動したときに送り直す分)
const RETAIN_SEGMENTS: usize = 1000;
/// partial を送る間隔の下限(チャンネルごと)
const PARTIAL_THROTTLE: Duration = Duration::from_millis(500);
/// 1 回の送信でまとめて送る文字起こしの上限(その間にライブ音声を挟めるように)
const EVENTS_PER_FLUSH: usize = 50;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_MAX: Duration = Duration::from_secs(15);
/// 終了時に未送信分を送り切るのを待つ上限
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// ライブ音声の送信待ち(1 要素 = 20 ms)。これ以上溜まったら捨てる
const LIVE_QUEUE: usize = 10;
/// 中継サーバとの往復がこれ以上なら「遅い」(黄)
const RTT_WARN_MS: u32 = 800;
/// 未送信がこれを超えたら「溜まっている」(黄)。区間録音は送っている最中の 1 件が常に数えられる
const BACKLOG_WARN: usize = 3;

/// 会場と中継サーバの間のメッセージ
pub(crate) mod proto {
    use crate::remote_server::{ChannelInfo, WireEvent, WireStatus};
    use anyhow::{bail, Result};
    use serde::{de::DeserializeOwned, Deserialize, Serialize};

    /// ライブ音声のデータグラムの先頭 1 バイト
    pub(crate) const LIVE: u8 = 2;
    /// ライブ音声のサンプリングレート(中継サーバはこのレートで復号してミックスする)
    pub(crate) const LIVE_RATE: u32 = 48000;
    /// ストリームの優先度(大きいほど先に送る)
    pub(crate) const CONTROL_PRIORITY: i32 = 10;
    pub(crate) const SEGMENT_PRIORITY: i32 = 0;
    /// 1 メッセージの上限
    pub(crate) const MAX_MESSAGE_BYTES: usize = 1 << 20;

    /// 会場 → 中継サーバ(制御ストリーム。`Seg` だけは区間録音のストリームの先頭)
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub(crate) enum Up {
        /// 接続直後に 1 回。`run` は会場の起動ごとに変わる
        Hello {
            run: String,
            /// 会場の anchor(UNIX ミリ秒)。イベントの `s` はここからの経過秒
            anchor_ms: u64,
            channels: Vec<ChannelInfo>,
            /// ライブ音声を送れるか
            live: bool,
        },
        /// 文字起こし。`q` は会場の連番
        Ev { q: u64, e: WireEvent },
        /// チャンネルの状態(全チャンネル分)
        St { v: Vec<WireStatus> },
        /// 区間録音の見出し。続けて中身 `n` バイトを送ってストリームを閉じる。`n` = 0 はファイルが読めなかった区間
        Seg {
            k: u64,
            /// 会場の区間 ID
            i: u64,
            c: usize,
            /// 開始時刻(UNIX ミリ秒)
            ts: u64,
            d: u64,
            n: u64,
            ext: String,
        },
        /// `Down::Tq` への返事。`b` は会場の UNIX ミリ秒、`pe` / `ps` は未送信の文字起こし・区間録音の数
        Tr { a: u64, b: u64, pe: u32, ps: u32 },
    }

    /// 中継サーバ → 会場(制御ストリーム)
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub(crate) enum Down {
        /// Hello への返事。受け取り済みの `q` / `k` と、閲覧画面のログインパス
        Welcome { q: u64, k: u64, login: String },
        /// 区間録音 `k` を受け取った(保存・登録済み)
        Ack { k: u64 },
        /// ライブ音声を送ってほしいチャンネル
        Want { c: Vec<usize> },
        /// 時刻の問い合わせ(`a` は中継サーバの UNIX ミリ秒)。往復時間と会場の時計のずれを測る
        Tq { a: u64 },
        /// 測った結果。`rtt` 往復(ms)、`vo` 会場の時計 − 中継サーバの時計、`so` 中継サーバの時計のずれ
        Lk { rtt: u32, vo: i64, so: Option<i64> },
    }

    /// `[長さ u32 BE][JSON]`
    pub(crate) fn frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
        let json = serde_json::to_vec(msg)?;
        let mut out = Vec::with_capacity(4 + json.len());
        out.extend_from_slice(&(json.len() as u32).to_be_bytes());
        out.extend_from_slice(&json);
        Ok(out)
    }

    pub(crate) async fn write_msg<T: Serialize>(send: &mut quinn::SendStream, msg: &T) -> Result<()> {
        send.write_all(&frame(msg)?).await?;
        Ok(())
    }

    /// 1 メッセージ読む。相手がストリームを閉じたら None
    pub(crate) async fn read_msg<T: DeserializeOwned>(recv: &mut quinn::RecvStream) -> Result<Option<T>> {
        let mut len = [0u8; 4];
        match recv.read_exact(&mut len).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_MESSAGE_BYTES {
            bail!("メッセージが大きすぎます ({} bytes)", len);
        }
        let mut buf = vec![0u8; len];
        recv.read_exact(&mut buf).await?;
        Ok(Some(serde_json::from_slice(&buf)?))
    }

    /// 制御ストリームを読み続けて channel に流す(`read_msg` は select! で途中キャンセルできないため別タスクで読む)
    pub(crate) fn spawn_reader<T: DeserializeOwned + Send + 'static>(
        mut recv: quinn::RecvStream,
    ) -> tokio::sync::mpsc::Receiver<Result<T>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let msg = read_msg::<T>(&mut recv).await;
                let end = !matches!(msg, Ok(Some(_)));
                let item = match msg {
                    Ok(Some(m)) => Ok(m),
                    Ok(None) => Err(anyhow::anyhow!("相手が制御ストリームを閉じました")),
                    Err(e) => Err(e),
                };
                if tx.send(item).await.is_err() || end {
                    break;
                }
            }
        });
        rx
    }

    /// `[LIVE][連番 u32][録った時刻 u64(UNIX ms、会場の時計)]` に続けて、
    /// チャンネルごとに `[ch u16][len u16][Opus パケット]`(全チャンネル同じ 20 ms)
    pub(crate) fn live_frame(seq: u32, at_ms: u64, packets: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![LIVE];
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&at_ms.to_le_bytes());
        for (ch, p) in packets {
            out.extend_from_slice(&(*ch as u16).to_le_bytes());
            out.extend_from_slice(&(p.len() as u16).to_le_bytes());
            out.extend_from_slice(p);
        }
        out
    }

    /// 連番・録った時刻と、チャンネルごとの Opus パケット
    pub(crate) type ParsedLive<'a> = (u32, u64, Vec<(usize, &'a [u8])>);

    pub(crate) fn parse_live_frame(b: &[u8]) -> Option<ParsedLive<'_>> {
        let rest = b.strip_prefix(&[LIVE])?;
        let seq = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
        let at_ms = u64::from_le_bytes(rest.get(4..12)?.try_into().ok()?);
        let mut rest = &rest[12..];
        let mut out = Vec::new();
        while !rest.is_empty() {
            if rest.len() < 4 {
                return None;
            }
            let ch = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            let len = u16::from_le_bytes([rest[2], rest[3]]) as usize;
            let packet = rest.get(4..4 + len)?;
            out.push((ch, packet));
            rest = &rest[4 + len..];
        }
        Some((seq, at_ms, out))
    }
}

use proto::{Down, Up};

/// 中継サーバとの接続状態(TUI 表示用)
#[derive(Debug, Clone, Copy, PartialEq)]
enum Link {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Default)]
struct Outbox {
    /// 送り直せるよう残している文字起こし(古い順)
    events: VecDeque<(u64, WireEvent)>,
    next_q: u64,
    /// 中継サーバへ送った最後の `q`
    sent_q: u64,
    segs: VecDeque<(u64, SegmentInfo)>,
    next_k: u64,
    /// 中継サーバが受け取りを確認した最後の `k`
    done_k: u64,
    /// 最後に送ることにした状態(チャンネルごと)
    status: HashMap<usize, WireStatus>,
    status_dirty: bool,
    last_partial: HashMap<usize, Instant>,
}

impl Outbox {
    fn pending_events(&self) -> usize {
        self.events.iter().filter(|(q, _)| *q > self.sent_q).count()
    }

    fn pending_segments(&self) -> usize {
        self.segs.iter().filter(|(k, _)| *k > self.done_k).count()
    }
}

pub struct Uplink {
    server: String,
    server_name: String,
    client: quinn::ClientConfig,
    run: String,
    anchor_ms: u64,
    channels: Vec<ChannelInfo>,
    include_partials: bool,
    retain_events: usize,
    /// ライブ音声を送れるか(設定で有効、かつ Opus で扱えるサンプリングレート)
    live: bool,
    sample_rate: u32,
    live_bitrate: i32,
    outbox: Mutex<Outbox>,
    wake: Notify,
    /// 中継サーバが聞きたいチャンネル
    wanted: Mutex<Vec<usize>>,
    /// 入力側がライブ用のフレームを作るか(つながっていて、聞きたいチャンネルがあるとき)
    live_active: Arc<AtomicBool>,
    link: Mutex<(Link, Instant)>,
    /// 中継サーバから受け取った閲覧画面のログインパス
    login: Mutex<Option<String>>,
    /// 中継サーバが測った回線品質(往復 ms、会場の時計 − 中継の時計、中継の時計のずれ)
    quality: Mutex<Option<(u32, i64, Option<i64>)>>,
    /// 一度でもつながったか、失敗したか(最初の接続待ちだけ灰、それ以外の接続待ちは赤)
    ever_connected: AtomicBool,
    ever_failed: AtomicBool,
    /// 最後に切れた理由(TUI に短く出す。証明書の食い違いなどに気付けるように)
    last_error: Mutex<Option<String>>,
}

impl Uplink {
    /// 設定の証明書ファイルを読んで作る
    pub fn new(
        config: &UplinkConfig,
        channels: &[ChannelConfig],
        start_time: SystemTime,
        sample_rate: u32,
    ) -> Result<Arc<Self>> {
        let client = crate::pki::client_config_from_files(&config.ca_file, &config.cert_file, &config.key_file)?;
        Self::with_client_config(config, channels, start_time, sample_rate, client)
    }

    pub(crate) fn with_client_config(
        config: &UplinkConfig,
        channels: &[ChannelConfig],
        start_time: SystemTime,
        sample_rate: u32,
        client: quinn::ClientConfig,
    ) -> Result<Arc<Self>> {
        let mut run = [0u8; 8];
        getrandom::fill(&mut run).context("OS の乱数を取得できません")?;
        let live_rate_ok = matches!(sample_rate, 8000 | 12000 | 16000 | 24000 | 48000);
        if config.live && !live_rate_ok {
            log::warn!(
                "アップリンク: サンプリングレート {} Hz は Opus で扱えないので、ライブ音声は送りません",
                sample_rate
            );
        }
        Ok(Arc::new(Self {
            server: config.server.clone(),
            server_name: config.effective_server_name(),
            client,
            run: run.iter().map(|b| format!("{:02x}", b)).collect(),
            anchor_ms: unix_ms(start_time),
            channels: channels
                .iter()
                .filter(|c| c.enabled)
                .map(|c| ChannelInfo { id: c.id, name: c.name.clone() })
                .collect(),
            include_partials: config.include_partials,
            retain_events: config.retain_events.max(1),
            live: config.live && live_rate_ok,
            sample_rate,
            live_bitrate: config.live_bitrate,
            outbox: Mutex::new(Outbox { next_q: 1, next_k: 1, ..Default::default() }),
            wake: Notify::new(),
            wanted: Mutex::new(Vec::new()),
            live_active: Arc::new(AtomicBool::new(false)),
            link: Mutex::new((Link::Connecting, Instant::now())),
            login: Mutex::new(None),
            quality: Mutex::new(None),
            ever_connected: AtomicBool::new(false),
            ever_failed: AtomicBool::new(false),
            last_error: Mutex::new(None),
        }))
    }

    /// ライブ音声を送るか(入力側の分岐を作るかどうか)
    pub fn live_enabled(&self) -> bool {
        self.live
    }

    pub fn live_active_flag(&self) -> Arc<AtomicBool> {
        self.live_active.clone()
    }

    /// 文字起こし結果を送る(同期、ノンブロッキング)
    pub fn push_transcript(&self, result: &TranscriptResult, segment: Option<u64>) {
        let mut ob = self.outbox.lock().unwrap();
        if result.is_partial {
            if !self.include_partials {
                return;
            }
            let now = Instant::now();
            if ob.last_partial.get(&result.channel).is_some_and(|t| now.duration_since(*t) < PARTIAL_THROTTLE) {
                return;
            }
            ob.last_partial.insert(result.channel, now);
        }
        let q = ob.next_q;
        ob.next_q += 1;
        ob.events.push_back((q, WireEvent::from_result(result, segment)));
        while ob.events.len() > self.retain_events {
            ob.events.pop_front();
        }
        drop(ob);
        self.wake.notify_one();
    }

    /// 録音を終えた区間を送る(同期、ノンブロッキング。中身は回線が空いているときに送る)
    pub fn push_segment(&self, info: &SegmentInfo) {
        let mut ob = self.outbox.lock().unwrap();
        let k = ob.next_k;
        ob.next_k += 1;
        ob.segs.push_back((k, info.clone()));
        while ob.segs.len() > RETAIN_SEGMENTS {
            ob.segs.pop_front();
        }
        drop(ob);
        self.wake.notify_one();
    }

    /// チャンネルの状態を送る(意味のある変化があったときだけ)
    pub fn push_status(&self, items: &[ChannelStatus]) {
        let mut ob = self.outbox.lock().unwrap();
        let mut changed = false;
        for st in items {
            let wire = st.wire();
            if status_changed(ob.status.get(&st.channel), &wire) {
                ob.status.insert(st.channel, wire);
                changed = true;
            }
        }
        if changed {
            ob.status_dirty = true;
            drop(ob);
            self.wake.notify_one();
        }
    }

    /// 閲覧画面のログインパス(中継サーバにつながるまでは None)
    pub fn login_path(&self) -> Option<String> {
        self.login.lock().unwrap().clone()
    }

    /// TUI に出す状態(色と 1 行)
    pub fn status(&self) -> (Health, String) {
        let (link, since) = *self.link.lock().unwrap();
        let ob = self.outbox.lock().unwrap();
        let (ev, seg) = (ob.pending_events(), ob.pending_segments());
        drop(ob);
        let quality = *self.quality.lock().unwrap();
        let first = !self.ever_connected.load(Ordering::Relaxed) && !self.ever_failed.load(Ordering::Relaxed);
        let (mut health, mut s) = match link {
            Link::Connected => (Health::Good, "中継: 接続中".to_string()),
            Link::Connecting if first => (Health::Unknown, "中継: 接続しています".to_string()),
            Link::Connecting | Link::Disconnected => {
                let mut s = format!("中継: 切断 {} 秒", since.elapsed().as_secs());
                if let Some(e) = self.last_error.lock().unwrap().as_deref() {
                    s.push_str(&format!("({})", e));
                }
                (Health::Bad, s)
            }
        };
        if link == Link::Connected {
            if let Some((rtt, vo, so)) = quality {
                s.push_str(&format!(" {}ms", rtt));
                if rtt >= RTT_WARN_MS {
                    health = Health::Warn;
                    s.push_str("(遅い)");
                }
                // 会場の時計 − 正しい時刻(中継サーバのずれが分かればそれで補正する)
                let venue_error = vo + so.unwrap_or(0);
                if venue_error.abs() > crate::clock::DEFAULT_WARN_MS {
                    health = Health::Warn;
                    s.push_str(&format!(" 時計{:+.1}s", venue_error as f64 / 1000.0));
                }
            }
        }
        if ev > 0 || seg > 0 {
            s.push_str(&format!(" 未送信 文字{} 録音{}", ev, seg));
            if health == Health::Good && (ev > BACKLOG_WARN || seg > BACKLOG_WARN) {
                health = Health::Warn;
            }
        }
        (health, s)
    }

    fn set_link(&self, link: Link) {
        let mut cur = self.link.lock().unwrap();
        if cur.0 != link {
            *cur = (link, Instant::now());
        }
    }

    fn set_wanted(&self, channels: Vec<usize>) {
        let mut wanted = self.wanted.lock().unwrap();
        if *wanted != channels {
            if !channels.is_empty() || !wanted.is_empty() {
                log::info!("アップリンク: ライブ音声を送るチャンネル {:?}", channels);
            }
            self.live_active.store(self.live && !channels.is_empty(), Ordering::Relaxed);
            *wanted = channels;
        }
    }

    /// 中継サーバとの接続を保ち続ける。`stop` が立つか送り手が無くなったら、
    /// 未送信分を送り切って(上限 `DRAIN_TIMEOUT`)終わる
    pub async fn run(self: Arc<Self>, mut live_rx: mpsc::Receiver<Vec<u8>>, mut stop: watch::Receiver<bool>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            if stopped(&stop) {
                break;
            }
            // 切れたあとの接続待ちは「切断」のまま(切れてからの秒数を数え続ける)
            if !self.ever_failed.load(Ordering::Relaxed) && !self.ever_connected.load(Ordering::Relaxed) {
                self.set_link(Link::Connecting);
            }
            let started = Instant::now();
            let result = self.session(&mut live_rx, &mut stop).await;
            self.set_wanted(Vec::new());
            match result {
                Ok(()) => break,
                Err(e) => {
                    log::warn!("アップリンク: 切断 ({:#})", e);
                    let reason: String = format!("{:#}", e).chars().take(60).collect();
                    *self.last_error.lock().unwrap() = Some(reason);
                    self.ever_failed.store(true, Ordering::Relaxed);
                    self.set_link(Link::Disconnected);
                }
            }
            if stopped(&stop) {
                break;
            }
            // しばらくつながっていたなら、すぐにつなぎ直す
            if started.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_secs(1);
            }
            let jitter = Duration::from_millis(u64::from(rand_u8()) * 4);
            tokio::select! {
                _ = tokio::time::sleep(backoff + jitter) => {}
                _ = stop.changed() => {}
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
        self.set_link(Link::Disconnected);
    }

    /// 1 回の接続。`stop` で送り切って終えたら Ok、切断はエラー
    async fn session(&self, live_rx: &mut mpsc::Receiver<Vec<u8>>, stop: &mut watch::Receiver<bool>) -> Result<()> {
        let addr: SocketAddr = tokio::net::lookup_host(&self.server)
            .await
            .with_context(|| format!("{} を引けません", self.server))?
            .next()
            .with_context(|| format!("{} のアドレスがありません", self.server))?;
        // 宛先と同じ種類(IPv4 / IPv6)のソケットを開く。IP が変わっても同じソケットのまま接続を続ける
        let bind: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse()?;
        let endpoint = quinn::Endpoint::client(bind).context("UDP ソケットを開けません")?;
        let connecting = endpoint.connect_with(self.client.clone(), addr, &self.server_name)?;
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
            .await
            .context("接続がタイムアウト")?
            .context("接続できません(証明書・宛先を確認)")?;
        let result = self.serve(&conn, live_rx, stop).await;
        conn.close(0u32.into(), b"bye");
        let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
        result
    }

    async fn serve(
        &self,
        conn: &quinn::Connection,
        live_rx: &mut mpsc::Receiver<Vec<u8>>,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<()> {
        let (mut send, recv) = conn.open_bi().await?;
        send.set_priority(proto::CONTROL_PRIORITY)?;
        let hello = Up::Hello {
            run: self.run.clone(),
            anchor_ms: self.anchor_ms,
            channels: self.channels.clone(),
            live: self.live,
        };
        proto::write_msg(&mut send, &hello).await?;
        let mut down = proto::spawn_reader::<Down>(recv);

        // Welcome を待つ
        let (q, k, login) = match tokio::time::timeout(CONNECT_TIMEOUT, down.recv()).await {
            Err(_) => bail!("中継サーバの応答がタイムアウト"),
            Ok(None) => bail!("中継サーバが接続を閉じました"),
            Ok(Some(Ok(Down::Welcome { q, k, login }))) => (q, k, login),
            Ok(Some(Ok(other))) => bail!("最初の応答が Welcome ではありません: {:?}", other),
            Ok(Some(Err(e))) => return Err(e),
        };
        {
            let mut ob = self.outbox.lock().unwrap();
            // 中継サーバが再起動していれば 0 から、そうでなければ続きから送る
            ob.sent_q = q.min(ob.next_q - 1);
            ob.done_k = k.min(ob.next_k - 1);
            ob.status_dirty = !ob.status.is_empty();
            log::info!(
                "アップリンク: 接続 {} (受け取り済み 文字 {} / 録音 {}、未送信 文字 {} / 録音 {})",
                conn.remote_address(),
                q,
                k,
                ob.pending_events(),
                ob.pending_segments()
            );
        }
        *self.login.lock().unwrap() = Some(login);
        *self.quality.lock().unwrap() = None;
        *self.last_error.lock().unwrap() = None;
        self.ever_connected.store(true, Ordering::Relaxed);
        self.set_link(Link::Connected);

        // 区間録音は 1 件ずつ。送っている(または受け取り確認待ちの)区間の k
        let mut uploading: Option<u64> = None;
        let (upload_err_tx, mut upload_err_rx) = mpsc::channel::<anyhow::Error>(1);
        let mut drain_until: Option<tokio::time::Instant> = None;
        let mut too_large_warned = false;
        // 接続前に溜まったライブ音声は古いので捨てる
        while live_rx.try_recv().is_ok() {}
        let mut live_open = !live_rx.is_closed();

        loop {
            self.flush(&mut send).await?;
            if uploading.is_none() {
                let next = {
                    let ob = self.outbox.lock().unwrap();
                    ob.segs.iter().find(|(k, _)| *k > ob.done_k).cloned()
                };
                if let Some((k, info)) = next {
                    uploading = Some(k);
                    let (conn, err_tx) = (conn.clone(), upload_err_tx.clone());
                    tokio::spawn(async move {
                        if let Err(e) = upload_segment(&conn, k, &info).await {
                            let _ = err_tx.send(e).await;
                        }
                    });
                }
            }
            if drain_until.is_some() && self.drained() {
                return Ok(());
            }
            tokio::select! {
                biased;
                msg = down.recv() => match msg {
                    Some(Ok(Down::Ack { k })) => {
                        let mut ob = self.outbox.lock().unwrap();
                        ob.done_k = ob.done_k.max(k);
                        if uploading == Some(k) {
                            uploading = None;
                        }
                    }
                    Some(Ok(Down::Want { c })) => self.set_wanted(c),
                    Some(Ok(Down::Tq { a })) => {
                        let (pe, ps) = {
                            let ob = self.outbox.lock().unwrap();
                            (ob.pending_events() as u32, ob.pending_segments() as u32)
                        };
                        proto::write_msg(&mut send, &Up::Tr { a, b: unix_ms(SystemTime::now()), pe, ps }).await?;
                    }
                    Some(Ok(Down::Lk { rtt, vo, so })) => *self.quality.lock().unwrap() = Some((rtt, vo, so)),
                    Some(Ok(Down::Welcome { .. })) => {}
                    Some(Err(e)) => return Err(e),
                    None => bail!("中継サーバが接続を閉じました"),
                },
                e = upload_err_rx.recv() => {
                    if let Some(e) = e {
                        return Err(e.context("区間録音を送れません"));
                    }
                }
                _ = self.wake.notified() => {}
                frame = live_rx.recv(), if live_open => match frame {
                    Some(frame) => match conn.send_datagram(frame.into()) {
                        Ok(()) => {}
                        Err(quinn::SendDatagramError::TooLarge) if !too_large_warned => {
                            too_large_warned = true;
                            log::warn!("アップリンク: ライブ音声のフレームがデータグラムに収まりません(チャンネル数・ビットレートを下げてください)");
                        }
                        Err(quinn::SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                        Err(_) => {}
                    },
                    None => live_open = false,
                },
                _ = stop.changed(), if drain_until.is_none() => {
                    drain_until = Some(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                }
                _ = async { tokio::time::sleep_until(drain_until.unwrap()).await }, if drain_until.is_some() => {
                    log::warn!("アップリンク: 未送信分を残して終了します");
                    return Ok(());
                }
                e = conn.closed() => bail!("接続が切れました: {}", e),
            }
        }
    }

    /// 状態と文字起こしを制御ストリームに書く
    async fn flush(&self, send: &mut quinn::SendStream) -> Result<()> {
        let (status, events) = {
            let mut ob = self.outbox.lock().unwrap();
            let status = std::mem::take(&mut ob.status_dirty).then(|| {
                let mut v: Vec<WireStatus> = ob.status.values().copied().collect();
                v.sort_unstable();
                v
            });
            let sent_q = ob.sent_q;
            let events: Vec<(u64, WireEvent)> = ob
                .events
                .iter()
                .filter(|(q, _)| *q > sent_q)
                .take(EVENTS_PER_FLUSH)
                .cloned()
                .collect();
            (status, events)
        };
        if let Some(v) = status {
            proto::write_msg(send, &Up::St { v }).await?;
        }
        for (q, e) in events {
            proto::write_msg(send, &Up::Ev { q, e }).await?;
            self.outbox.lock().unwrap().sent_q = q;
        }
        Ok(())
    }

    /// 未送信の文字起こしも区間録音も無いか
    fn drained(&self) -> bool {
        let ob = self.outbox.lock().unwrap();
        ob.pending_events() == 0 && ob.pending_segments() == 0
    }

    /// 入力側のライブ用フレームを、聞かれているチャンネルだけ 20 ms ごとに Opus にして `out` へ送る
    pub async fn run_live_encoder(self: Arc<Self>, mut rx: mpsc::Receiver<LiveFrame>, out: mpsc::Sender<Vec<u8>>) {
        let frame_size = self.sample_rate as usize / 50;
        let mut encoders: HashMap<usize, (opus::Encoder, Vec<i16>)> = HashMap::new();
        let mut current: Vec<usize> = Vec::new();
        let mut packet = vec![0u8; 1500];
        let mut seq: u32 = 0;
        while let Some(frame) = rx.recv().await {
            let wanted = self.wanted.lock().unwrap().clone();
            if wanted != current {
                // チャンネル間で 20 ms の区切りがそろうよう、組み合わせが変わったら溜めていた分を捨てる
                encoders.retain(|ch, _| wanted.contains(ch));
                for (_, buf) in encoders.values_mut() {
                    buf.clear();
                }
                current = wanted;
            }
            if current.is_empty() {
                continue;
            }
            let at_ms = frame.at_ms;
            for (ch, samples) in &frame.channels {
                if !current.contains(ch) {
                    continue;
                }
                let entry = match encoders.entry(*ch) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => match self.new_opus_encoder() {
                        Ok(enc) => v.insert((enc, Vec::with_capacity(frame_size * 2))),
                        Err(e) => {
                            log::error!("アップリンク: Opus エンコーダを作れません: {:#}", e);
                            continue;
                        }
                    },
                };
                entry.1.extend_from_slice(samples);
            }
            while !encoders.is_empty() && encoders.values().all(|(_, buf)| buf.len() >= frame_size) {
                let mut packets = Vec::with_capacity(encoders.len());
                for (ch, (enc, buf)) in encoders.iter_mut() {
                    match enc.encode(&buf[..frame_size], &mut packet) {
                        Ok(len) => packets.push((*ch, packet[..len].to_vec())),
                        Err(e) => log::debug!("アップリンク: Opus エンコード失敗 ch{}: {:?}", ch, e),
                    }
                    buf.drain(..frame_size);
                }
                packets.sort_unstable_by_key(|(ch, _)| *ch);
                // このフレームの最後のサンプルを録った時刻 = 入力の時刻 − まだ溜まっている分
                let left = encoders.values().next().map_or(0, |(_, b)| b.len()) as u64;
                let frame_at = if at_ms == 0 { 0 } else { at_ms.saturating_sub(left * 1000 / self.sample_rate as u64) };
                // 回線が詰まっていたら捨てる(遅延を溜めない)。連番は捨てた分も進める(中継サーバが欠けを補う)
                let _ = out.try_send(proto::live_frame(seq, frame_at, &packets));
                seq = seq.wrapping_add(1);
            }
        }
    }

    fn new_opus_encoder(&self) -> Result<opus::Encoder> {
        let e = |e: opus::Error| anyhow::anyhow!("{:?}", e);
        let mut enc = opus::Encoder::new(self.sample_rate, opus::Channels::Mono, opus::Application::Voip).map_err(e)?;
        enc.set_bitrate(opus::Bitrate::Bits(self.live_bitrate)).map_err(e)?;
        // 1 フレーム落ちても次のパケットから復元できるよう、前のフレームの情報を少し載せる
        enc.set_inband_fec(true).map_err(e)?;
        enc.set_packet_loss_perc(10).map_err(e)?;
        Ok(enc)
    }
}

/// 区間録音 1 件を単方向ストリームで送る(見出し + 中身)。受け取り確認は制御ストリームの `Ack` で来る
async fn upload_segment(conn: &quinn::Connection, k: u64, info: &SegmentInfo) -> Result<()> {
    let data = match tokio::fs::read(&info.path).await {
        Ok(d) => d,
        Err(e) => {
            log::warn!("アップリンク: 区間録音を読めません {:?}: {}", info.path, e);
            Vec::new()
        }
    };
    let ext = match info.path.extension().and_then(|e| e.to_str()) {
        Some("wav") => "wav",
        _ => "aac",
    };
    let header = Up::Seg {
        k,
        i: info.id,
        c: info.channel,
        ts: info.start_unix_ms,
        d: info.duration_ms,
        n: data.len() as u64,
        ext: ext.to_string(),
    };
    let mut send = conn.open_uni().await?;
    send.set_priority(proto::SEGMENT_PRIORITY)?;
    proto::write_msg(&mut send, &header).await?;
    send.write_all(&data).await?;
    send.finish()?;
    Ok(())
}

/// ライブ音声の送信待ちキュー
pub fn live_queue() -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(LIVE_QUEUE)
}

/// 閲覧画面の公開 URL の先頭(`https://host`)。会場から中継サーバへの宛先とは別なので、`public_url` で指定する
pub fn public_base(config: &UplinkConfig) -> Option<String> {
    let configured = config.public_url.trim().trim_end_matches('/');
    (!configured.is_empty()).then(|| configured.to_string())
}

/// 停止が指示されたか(送り手が無くなったのも停止とみなす。engine がエラーで抜けたとき)
fn stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow() || stop.has_changed().is_err()
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn rand_u8() -> u8 {
    let mut b = [0u8; 1];
    let _ = getrandom::fill(&mut b);
    b[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_frame_roundtrip() {
        let b = proto::live_frame(7, 1_700_000_000_123, &[(2, vec![1, 2, 3]), (5, vec![])]);
        let (seq, at, parsed) = proto::parse_live_frame(&b).unwrap();
        assert_eq!((seq, at), (7, 1_700_000_000_123));
        assert_eq!(parsed, vec![(2, &[1u8, 2, 3][..]), (5, &[][..])]);
        // 長さが足りないものは受け付けない
        assert!(proto::parse_live_frame(&b[..b.len() - 6]).is_none());
        assert!(proto::parse_live_frame(&[proto::LIVE, 0, 0, 0, 0, 0]).is_none());
        assert!(proto::parse_live_frame(&[9, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn messages_are_tagged_json_with_length_prefix() {
        let up = Up::Ev {
            q: 3,
            e: WireEvent { c: 2, s: 1.5, x: "了解".into(), g: Some(9), ..Default::default() },
        };
        let json = serde_json::to_string(&up).unwrap();
        assert_eq!(json, r#"{"t":"ev","q":3,"e":{"c":2,"s":1.5,"x":"了解","g":9}}"#);
        assert_eq!(serde_json::from_str::<Up>(&json).unwrap(), up);
        let framed = proto::frame(&up).unwrap();
        assert_eq!(u32::from_be_bytes(framed[..4].try_into().unwrap()) as usize, json.len());
        let down: Down = serde_json::from_str(r#"{"t":"want","c":[2,3]}"#).unwrap();
        assert_eq!(down, Down::Want { c: vec![2, 3] });
    }

    #[test]
    fn public_base_is_explicit() {
        let mut c = UplinkConfig { server: "203.0.113.5:443".into(), ..Default::default() };
        assert_eq!(public_base(&c), None);
        c.public_url = "https://view.example.com/".into();
        assert_eq!(public_base(&c).as_deref(), Some("https://view.example.com"));
    }

    fn uplink(include_partials: bool) -> Arc<Uplink> {
        let set = crate::pki::issue(&["127.0.0.1".into()], 30).unwrap();
        let client = crate::pki::client_config(set.ca_pem.as_bytes(), set.venue_pem.as_bytes(), set.venue_key.as_bytes())
            .unwrap();
        let cfg = UplinkConfig {
            enabled: true,
            server: "127.0.0.1:1".into(),
            include_partials,
            retain_events: 3,
            ..Default::default()
        };
        Uplink::with_client_config(&cfg, &[], SystemTime::now(), 48000, client).unwrap()
    }

    #[test]
    fn outbox_numbers_events_and_keeps_recent() {
        let u = uplink(false);
        for i in 0..5 {
            u.push_transcript(&TranscriptResult::new(0, format!("m{}", i), false, None, SystemTime::now()), None);
        }
        // partial は送らない設定
        u.push_transcript(&TranscriptResult::new(0, "p".into(), true, None, SystemTime::now()), None);
        let ob = u.outbox.lock().unwrap();
        let qs: Vec<u64> = ob.events.iter().map(|(q, _)| *q).collect();
        assert_eq!(qs, vec![3, 4, 5]);
        assert_eq!(ob.pending_events(), 3);
    }

    #[test]
    fn status_marked_dirty_only_on_change() {
        let u = uplink(false);
        let st = |l: f32| ChannelStatus { channel: 0, level_db: l, receiving: false, transcribe: 1 };
        u.push_status(&[st(-50.0)]);
        assert!(std::mem::take(&mut u.outbox.lock().unwrap().status_dirty));
        u.push_status(&[st(-49.0)]);
        assert!(!u.outbox.lock().unwrap().status_dirty);
    }
}
