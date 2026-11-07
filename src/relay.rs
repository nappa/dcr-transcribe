//! 中継サーバ(`dcr-transcribe --relay`)
//!
//! 会場からのアップリンク(`crate::uplink`、QUIC + mTLS)を受けて、閲覧画面の配信(`crate::remote_server`、
//! cloudflared 経由の HTTPS)をこちらで行う。閲覧者が何人いても会場の上り回線は 1 本分で済み、
//! 会場の回線が切れても閲覧画面・履歴・録音は見られる。
//!
//! ```text
//! [会場] ══ QUIC (UDP, mTLS) ══> [Relay] ── push_event / register_segment / LiveHub ──> [RemoteState] ──> 閲覧者
//! ```
//!
//! - QUIC の口は `[relay] listen`。私設 CA が署名したクライアント証明書を出せない相手はハンドシェイクで落ちる
//! - 送信元を偽った大量の接続要求を避けるため、まだ確かめていない送信元には retry(アドレス確認)を返す
//! - 中継サーバの起動時刻を anchor にし、会場の時刻(`s`)を付け替える。会場が再起動しても
//!   閲覧画面の履歴は続いたまま
//! - 区間 ID は会場の起動ごとに別の範囲へずらす(`seg_base`)。文字起こしの `g` も同じだけずらす
//! - 会場は同時に 1 つだけ。新しい接続が来たら古い接続を閉じる
//! - ライブ音声はデータグラム。1 フレームの欠けは Opus の FEC で、数フレームの欠けは補間で埋める
//! - 会場との接続状態は `event: up` で閲覧画面に知らせる

use crate::clock::ClockMonitor;
use crate::config::Config;
use crate::live::{LiveFrame, LiveHub};
use crate::remote_server::{self, RemoteState, UpState, WireEvent};
use crate::uplink::proto::{self, Down, Up};
use anyhow::{Context, Result};
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, watch};

/// 聞きたいチャンネルを確かめる間隔
const WANT_INTERVAL: Duration = Duration::from_millis(250);
/// 会場との往復時間・時計のずれを測る間隔
const PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// ハンドシェイク・Hello を待つ上限
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// 1 区間の上限(HE-AAC 24kbps × 300 秒 ≒ 0.9MB、WAV なら大きくなる)
const MAX_SEGMENT_BYTES: u64 = 64 << 20;
/// ライブ音声の欠けを補間で埋める上限(フレーム数 = 20 ms 単位)。これより長い欠けは無音のまま
const MAX_CONCEAL_FRAMES: u32 = 5;
/// Opus の 20 ms(48 kHz)
const LIVE_FRAME_SAMPLES: usize = proto::LIVE_RATE as usize / 50;

/// 会場の 1 回の起動
#[derive(Debug, Default)]
struct Run {
    run: String,
    /// 会場の anchor から中継サーバの anchor への差(秒)。会場の `s` に足す
    offset_s: f64,
    /// 会場の区間 ID に足して中継サーバの区間 ID にする
    seg_base: u64,
    /// 受け取り済みの文字起こしの連番
    last_q: u64,
    /// 受け取り済みの区間録音の連番
    last_k: u64,
}

pub struct Relay {
    state: Arc<RemoteState>,
    live: Option<Arc<LiveHub>>,
    dir: PathBuf,
    anchor_ms: u64,
    run: Mutex<Run>,
    /// これまでに振った区間 ID の最大(会場が再起動したら、この続きから振る)
    max_seg: Mutex<u64>,
    /// 接続ごとに増える番号。増えたら古い接続は閉じる
    generation: watch::Sender<u64>,
    clock: Arc<ClockMonitor>,
    /// ハンドシェイクに失敗した数(証明書の無い相手・ポートスキャンなど)。1 分ごとにまとめてログに出す
    handshake_failures: AtomicU64,
}

/// 中継サーバを起動し、`running` が落ちるまで動かす
pub async fn run(config: Config, running: Arc<AtomicBool>) -> Result<()> {
    log::info!("中継サーバを起動します(メモリアロケータ: {})", crate::allocator_desc());
    let remote = config
        .remote
        .clone()
        .unwrap_or_else(|| toml::from_str("").expect("RemoteConfig の既定値"));
    let start_time = SystemTime::now();
    let live = config
        .live
        .enabled
        .then(|| LiveHub::new(proto::LIVE_RATE, config.live.bitrate, config.live.profile, Vec::new()));
    let clock = ClockMonitor::new();
    tokio::spawn(clock.clone().run(config.clock.clone(), running.clone()));
    let state = Arc::new(remote_server::build(
        &remote,
        Vec::new(),
        start_time,
        true,
        live.clone(),
        Some(clock.clone()),
        true,
    )?);

    let dir = PathBuf::from(&config.relay.dir).join(
        chrono::DateTime::<chrono::Local>::from(start_time)
            .format("%Y%m%d_%H%M%S")
            .to_string(),
    );
    std::fs::create_dir_all(&dir).with_context(|| format!("ディレクトリを作れません: {:?}", dir))?;

    let quic = crate::pki::server_config_from_files(
        &config.relay.cert_file,
        &config.relay.key_file,
        &config.relay.client_ca_file,
    )
    .context("[relay] の証明書を読めません(`--gen-uplink-pki` で作る)")?;
    let listen: SocketAddr = config
        .relay
        .listen
        .parse()
        .with_context(|| format!("[relay] listen が不正です: {}", config.relay.listen))?;

    let relay = Relay::new(state.clone(), live, dir, start_time, clock);
    let endpoint = relay.clone().listen(quic, listen)?;
    remote_server::serve(state.clone(), &remote, Router::new()).await?;
    println!("中継サーバを起動しました: 閲覧 http://{} / 会場 quic {}", remote.listen, listen);
    println!("ログイン URL は `dcr-transcribe --show-login-url <設定ファイル>` で表示できます");

    let tunnel = if config.tunnel.enabled {
        Some(crate::tunnel::Tunnel::start(&config.tunnel).context("トンネルの設定が不正です")?)
    } else {
        None
    };
    while running.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    endpoint.close(0u32.into(), b"shutdown");
    if let Some(t) = tunnel {
        t.stop().await;
    }
    Ok(())
}

impl Relay {
    fn new(
        state: Arc<RemoteState>,
        live: Option<Arc<LiveHub>>,
        dir: PathBuf,
        start_time: SystemTime,
        clock: Arc<ClockMonitor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            live,
            dir,
            anchor_ms: unix_ms(start_time),
            run: Mutex::new(Run::default()),
            max_seg: Mutex::new(0),
            generation: watch::channel(0).0,
            clock,
            handshake_failures: AtomicU64::new(0),
        })
    }

    /// QUIC の口を開いて受け付けを始める
    fn listen(self: Arc<Self>, config: quinn::ServerConfig, addr: SocketAddr) -> Result<quinn::Endpoint> {
        let endpoint =
            quinn::Endpoint::server(config, addr).with_context(|| format!("QUIC の口を開けません: {}", addr))?;
        log::info!("中継サーバ: 会場からの QUIC を {} で待ちます", endpoint.local_addr()?);
        tokio::spawn(self.clone().accept_loop(endpoint.clone()));
        tokio::spawn(self.report_failures());
        Ok(endpoint)
    }

    async fn accept_loop(self: Arc<Self>, endpoint: quinn::Endpoint) {
        while let Some(incoming) = endpoint.accept().await {
            // 送信元をまだ確かめていなければ retry を返す(偽った送信元で中継サーバに応答を撃たせない)
            if !incoming.remote_address_validated() {
                let _ = incoming.retry();
                continue;
            }
            let remote = incoming.remote_address();
            let relay = self.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
                    Ok(Ok(conn)) => {
                        log::info!("中継サーバ: 会場が接続 {}", remote);
                        if let Err(e) = relay.session(conn).await {
                            log::warn!("中継サーバ: 会場との接続が切れました ({:#})", e);
                        }
                    }
                    Ok(Err(e)) => {
                        relay.handshake_failures.fetch_add(1, Ordering::Relaxed);
                        log::debug!("中継サーバ: ハンドシェイク失敗 {} ({})", remote, e);
                    }
                    Err(_) => {
                        relay.handshake_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    }

    /// 証明書の無い接続などの失敗を 1 分ごとにまとめて出す(ポートスキャンでログが埋まらないように)
    async fn report_failures(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let n = self.handshake_failures.swap(0, Ordering::Relaxed);
            if n > 0 {
                log::info!("中継サーバ: この 1 分でハンドシェイクに失敗した接続 {} 件(証明書なし・期限切れなど)", n);
            }
        }
    }

    async fn session(self: Arc<Self>, conn: quinn::Connection) -> Result<()> {
        // 古い接続を閉じさせる
        self.generation.send_modify(|g| *g += 1);
        let mut gen_rx = self.generation.subscribe();
        let me = *gen_rx.borrow_and_update();

        let (mut send, recv) = tokio::time::timeout(HELLO_TIMEOUT, conn.accept_bi())
            .await
            .context("制御ストリームが来ません")??;
        send.set_priority(proto::CONTROL_PRIORITY)?;
        let mut up_rx = proto::spawn_reader::<Up>(recv);
        let Some(Ok(Up::Hello { run, anchor_ms, channels, live })) =
            tokio::time::timeout(HELLO_TIMEOUT, up_rx.recv()).await.context("Hello がタイムアウト")?
        else {
            anyhow::bail!("最初のメッセージが Hello ではありません");
        };

        let (q, k) = {
            let mut cur = self.run.lock().unwrap();
            if cur.run != run {
                let seg_base = *self.max_seg.lock().unwrap();
                log::info!("中継サーバ: 会場の新しい起動 {} (区間 ID は {} から)", run, seg_base + 1);
                *cur = Run {
                    run,
                    offset_s: (anchor_ms as f64 - self.anchor_ms as f64) / 1000.0,
                    seg_base,
                    last_q: 0,
                    last_k: 0,
                };
            }
            (cur.last_q, cur.last_k)
        };
        log::info!("中継サーバ: 会場の受け取り済み 文字 {} / 録音 {}", q, k);
        if let Some(hub) = &self.live {
            hub.set_channels(channels.iter().map(|c| c.id).collect());
        }
        self.state.set_channels(channels);
        self.state.set_live_source(live);
        // 切断中に止まって見えていた状態を、会場から届く次の状態で全チャンネル分更新させる
        self.state.clear_status();
        proto::write_msg(&mut send, &Down::Welcome { q, k, login: self.state.login_path() }).await?;
        self.state.set_upstream(UpState { on: true, ..Default::default() });

        let result = self.serve(&conn, &mut send, &mut up_rx, &mut gen_rx, me, live).await;
        // 新しい接続に置き換わったのでなければ、閲覧画面に切断を知らせる
        if *self.generation.borrow() == me {
            self.state.set_upstream(UpState { on: false, t: Some(unix_ms(SystemTime::now())), ..Default::default() });
        }
        conn.close(0u32.into(), b"bye");
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve(
        self: &Arc<Self>,
        conn: &quinn::Connection,
        send: &mut quinn::SendStream,
        up_rx: &mut mpsc::Receiver<Result<Up>>,
        gen_rx: &mut watch::Receiver<u64>,
        me: u64,
        live: bool,
    ) -> Result<()> {
        let mut live_rx = LiveReceiver::default();
        let mut want_tick = tokio::time::interval(WANT_INTERVAL);
        let mut ticks: u32 = 0;
        let mut wanted_sent: Option<Vec<usize>> = None;
        let mut probe = tokio::time::interval(PROBE_INTERVAL);
        let mut clock_warned = false;
        let mut remote = conn.remote_address();
        // 区間録音を受け取り終えたら、その k をここへ(制御ストリームで Ack を返す)
        let (seg_tx, mut seg_rx) = mpsc::channel::<u64>(4);
        loop {
            tokio::select! {
                msg = up_rx.recv() => match msg {
                    Some(Ok(Up::Tr { a, b, pe, ps })) => {
                        let (lk, vo) = self.on_probe_reply(a, b, pe, ps, live_rx.delay_ms(), &mut clock_warned);
                        live_rx.vo = Some(vo);
                        proto::write_msg(send, &lk).await?;
                    }
                    Some(Ok(up)) => self.on_message(up),
                    Some(Err(e)) => return Err(e),
                    None => return Ok(()),
                },
                stream = conn.accept_uni() => {
                    let recv = stream.context("会場との接続が切れました")?;
                    let (relay, tx) = (self.clone(), seg_tx.clone());
                    tokio::spawn(async move {
                        match relay.receive_segment(recv).await {
                            Ok(Some(k)) => {
                                let _ = tx.send(k).await;
                            }
                            Ok(None) => {}
                            Err(e) => log::warn!("中継サーバ: 区間録音を受け取れません ({:#})", e),
                        }
                    });
                }
                Some(k) = seg_rx.recv() => proto::write_msg(send, &Down::Ack { k }).await?,
                dg = conn.read_datagram() => {
                    let dg = dg.context("会場との接続が切れました")?;
                    if let Some(hub) = &self.live {
                        live_rx.on_datagram(&dg, hub);
                    }
                }
                _ = want_tick.tick(), if live && self.live.is_some() => {
                    // ライブ音声の遅延は 2 秒ごとに閲覧画面へ(聞いている人がいるときだけ値が入る)
                    ticks = ticks.wrapping_add(1);
                    if ticks % 8 == 0 {
                        let ld = live_rx.delay_ms();
                        self.state.update_upstream(|u| u.ld = ld);
                    }
                    let wanted = self.live.as_ref().map(|h| h.wanted_channels()).unwrap_or_default();
                    if wanted_sent.as_ref() != Some(&wanted) {
                        proto::write_msg(send, &Down::Want { c: wanted.clone() }).await?;
                        wanted_sent = Some(wanted);
                    }
                }
                _ = probe.tick() => {
                    proto::write_msg(send, &Down::Tq { a: unix_ms(SystemTime::now()) }).await?;
                    // 会場の IP が変わっても QUIC は接続を続ける(移行)。分かるようにログに残す
                    let now = conn.remote_address();
                    if now != remote {
                        log::info!("中継サーバ: 会場のアドレスが変わりました {} → {}(接続は継続)", remote, now);
                        remote = now;
                    }
                }
                r = gen_rx.changed() => {
                    if r.is_err() || *gen_rx.borrow() != me {
                        log::info!("中継サーバ: 新しい接続に置き換えます");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn on_message(&self, up: Up) {
        match up {
            Up::Ev { q, e } => {
                let mut run = self.run.lock().unwrap();
                if q <= run.last_q {
                    return;
                }
                run.last_q = q;
                let g = e.g.map(|g| run.seg_base + g);
                // 時刻は中継サーバの anchor からの経過に付け替える(`r` は会場の時計の UNIX ミリ秒のまま)
                let event = WireEvent { s: e.s + run.offset_s, e: e.e.map(|x| x + run.offset_s), g, ..e };
                drop(run);
                if let Some(g) = g {
                    self.note_seg_id(g);
                }
                self.state.push_event(event);
            }
            Up::St { v } => self.state.push_wire_status(&v),
            Up::Hello { .. } | Up::Tr { .. } | Up::Seg { .. } => {}
        }
    }

    /// 区間録音のストリームを 1 本受け取る。受け取り確認を返すべき k を返す
    async fn receive_segment(&self, mut recv: quinn::RecvStream) -> Result<Option<u64>> {
        let Some(Up::Seg { k, i, c, ts, d, n, ext }) = proto::read_msg::<Up>(&mut recv).await? else {
            anyhow::bail!("区間録音の見出しがありません");
        };
        let (dup, id) = {
            let run = self.run.lock().unwrap();
            (k <= run.last_k, run.seg_base + i)
        };
        // 受け取り済み(つなぎ直しの重複)・中身の無い区間・大きすぎる区間は、受け取ったことにして先へ進ませる
        if dup || n == 0 || n > MAX_SEGMENT_BYTES {
            let _ = recv.stop(0u32.into());
            if !dup {
                if n > 0 {
                    log::warn!("中継サーバ: 大きすぎる区間録音を捨てます ({} bytes)", n);
                }
                let mut run = self.run.lock().unwrap();
                run.last_k = run.last_k.max(k);
            }
            return Ok(Some(k));
        }
        let data = recv.read_to_end(n as usize).await.context("区間録音の中身を読めません")?;
        anyhow::ensure!(data.len() as u64 == n, "区間録音が途中で切れました ({} / {} bytes)", data.len(), n);
        let path = self.dir.join(format!("{}.{}", id, if ext == "wav" { "wav" } else { "aac" }));
        // 書けなかったら受け取り確認を返さない(会場はつなぎ直したときに送り直す)
        tokio::fs::write(&path, &data)
            .await
            .with_context(|| format!("区間録音を保存できません {:?}", path))?;
        self.note_seg_id(id);
        self.state.register_segment(id, c, ts, d, path);
        let mut run = self.run.lock().unwrap();
        run.last_k = run.last_k.max(k);
        Ok(Some(k))
    }

    /// 時刻の問い合わせへの返事から、往復時間と会場の時計のずれを求めて閲覧画面に知らせる
    /// 戻り値の 2 つめは 会場の時計 − 中継サーバの時計(ライブ音声の遅延の補正に使う)
    fn on_probe_reply(&self, a: u64, b: u64, pe: u32, ps: u32, ld: Option<u32>, warned: &mut bool) -> (Down, i64) {
        let c = unix_ms(SystemTime::now());
        let rtt = c.saturating_sub(a);
        // 会場の時計 − 中継サーバの時計(往復の真ん中で会場が答えたとみなす)
        let vo = b as i64 - (a + c) as i64 / 2;
        let so = self.clock.offset_ms();
        // 中継サーバ自身のずれが分かれば、会場の時計 − 正しい時刻 = vo + so
        let venue_error = vo + so.unwrap_or(0);
        let bad = venue_error.abs() > self.clock.warn_ms();
        if bad && !*warned {
            log::warn!("中継サーバ: 会場の時計が {:+} ms ずれています(往復 {} ms)", venue_error, rtt);
        } else if !bad && *warned {
            log::info!("中継サーバ: 会場の時計のずれが戻りました ({:+} ms)", venue_error);
        }
        *warned = bad;
        self.state.set_upstream(UpState {
            on: true,
            t: None,
            rtt: Some(rtt.min(u32::MAX as u64) as u32),
            q: Some(pe),
            k: Some(ps),
            vo: Some(vo),
            ld,
        });
        (Down::Lk { rtt: rtt.min(u32::MAX as u64) as u32, vo, so }, vo)
    }

    fn note_seg_id(&self, id: u64) {
        let mut max = self.max_seg.lock().unwrap();
        *max = (*max).max(id);
    }
}

/// ライブ音声のデータグラムを復号して LiveHub に流す。欠けたフレームは FEC / 補間で埋める
#[derive(Default)]
struct LiveReceiver {
    decoders: HashMap<usize, opus::Decoder>,
    last_seq: Option<u32>,
    /// 会場の時計 − 中継サーバの時計(時刻の問い合わせで分かる)
    vo: Option<i64>,
    /// 会場で録ってから届くまで(ms、なめらかにした値)と、最後に測った時刻
    delay: Option<f64>,
    delay_at: Option<std::time::Instant>,
    /// 届かずに埋めたフレーム数(デバッグ用)
    concealed: u64,
}

impl LiveReceiver {
    /// 会場で録ってから中継サーバに届くまで(ms)。3 秒届いていなければ None
    fn delay_ms(&self) -> Option<u32> {
        let fresh = self.delay_at.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
        self.delay.filter(|_| fresh).map(|d| d.round().max(0.0) as u32)
    }

    fn on_datagram(&mut self, b: &[u8], hub: &LiveHub) {
        let Some((seq, at_ms, packets)) = proto::parse_live_frame(b) else { return };
        // 片道 = 届いた時刻 − 録った時刻(会場の時計を中継サーバの時計に直して)。20 ms ごとに来るので 1 秒ほどでならす
        if let (true, Some(vo)) = (at_ms > 0, self.vo) {
            let one_way = unix_ms(SystemTime::now()) as f64 - (at_ms as f64 - vo as f64);
            self.delay = Some(match self.delay {
                Some(d) => d + (one_way - d) * 0.05,
                None => one_way,
            });
            self.delay_at = Some(std::time::Instant::now());
        }
        // 連番が前より小さい(遅れて届いた・重複)ものは捨てる。2^31 以上離れていれば一周したとみなす
        let gap = match self.last_seq {
            Some(last) => {
                let d = seq.wrapping_sub(last);
                if d == 0 || d > u32::MAX / 2 {
                    return;
                }
                d - 1
            }
            None => 0,
        };
        self.last_seq = Some(seq);
        let mut pcm = [0i16; LIVE_FRAME_SAMPLES];

        if gap > 0 && gap <= MAX_CONCEAL_FRAMES {
            // 直前の 1 フレームは今のパケットに載っている FEC で復元し、それより前は補間する
            for lost in (0..gap).rev() {
                let mut channels = Vec::with_capacity(packets.len());
                for (ch, packet) in &packets {
                    let Some(dec) = self.decoder(*ch) else { continue };
                    let r = if lost == 0 { dec.decode(packet, &mut pcm, true) } else { dec.decode(&[], &mut pcm, false) };
                    if let Ok(n) = r {
                        channels.push((*ch, pcm[..n].to_vec()));
                    }
                }
                self.concealed += 1;
                if !channels.is_empty() {
                    hub.process(&LiveFrame { channels, at_ms });
                }
            }
        }

        let mut channels = Vec::with_capacity(packets.len());
        for (ch, packet) in &packets {
            let Some(dec) = self.decoder(*ch) else { continue };
            match dec.decode(packet, &mut pcm, false) {
                Ok(n) => channels.push((*ch, pcm[..n].to_vec())),
                Err(e) => log::debug!("中継サーバ: Opus 復号失敗 ch{}: {:?}", ch, e),
            }
        }
        if !channels.is_empty() {
            hub.process(&LiveFrame { channels, at_ms });
        }
    }

    fn decoder(&mut self, ch: usize) -> Option<&mut opus::Decoder> {
        match self.decoders.entry(ch) {
            std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(v) => match opus::Decoder::new(proto::LIVE_RATE, opus::Channels::Mono) {
                Ok(d) => Some(v.insert(d)),
                Err(e) => {
                    log::error!("中継サーバ: Opus デコーダを作れません: {:?}", e);
                    None
                }
            },
        }
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::AacProfile;
    use crate::config::{ChannelConfig, RemoteConfig, UplinkConfig};
    use crate::pki::Issued;
    use crate::segment_recorder::SegmentInfo;
    use crate::types::TranscriptResult;
    use crate::uplink::Uplink;
    use futures_util::StreamExt;

    fn remote_cfg() -> RemoteConfig {
        let mut c: RemoteConfig = toml::from_str("").unwrap();
        c.listen = "127.0.0.1:0".into();
        c.login_key_file = String::new();
        // ここは中継の仕組みのテスト。ブラウザの Sec-Fetch-* の規則は remote_server のテストで確かめる
        c.require_fetch_metadata = false;
        c.browser.enabled = false;
        c
    }

    struct Fixture {
        http: SocketAddr,
        quic: SocketAddr,
        pki: Issued,
        _endpoint: quinn::Endpoint,
    }

    async fn start_relay(dir: &std::path::Path, live: bool) -> Fixture {
        let hub = live.then(|| LiveHub::new(proto::LIVE_RATE, 16000, AacProfile::He, Vec::new()));
        let start = SystemTime::now();
        let state = Arc::new(remote_server::build(&remote_cfg(), Vec::new(), start, true, hub.clone(), None, true).unwrap());
        let pki = crate::pki::issue(&["127.0.0.1".into()], 30).unwrap();
        let quic_cfg =
            crate::pki::server_config(pki.relay_pem.as_bytes(), pki.relay_key.as_bytes(), pki.ca_pem.as_bytes()).unwrap();
        let relay = Relay::new(state.clone(), hub, dir.to_path_buf(), start, ClockMonitor::new());
        let endpoint = relay.listen(quic_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let quic = endpoint.local_addr().unwrap();
        let http = remote_server::serve(state, &remote_cfg(), Router::new()).await.unwrap();
        Fixture { http, quic, pki, _endpoint: endpoint }
    }

    fn venue_with(f: &Fixture, ca: &str, cert: &str, key: &str, live: bool) -> Arc<Uplink> {
        let cfg = UplinkConfig { enabled: true, server: f.quic.to_string(), live, ..Default::default() };
        let client = crate::pki::client_config(ca.as_bytes(), cert.as_bytes(), key.as_bytes()).unwrap();
        let channels = [ChannelConfig { id: 2, name: "本部".into(), enabled: true }];
        Uplink::with_client_config(&cfg, &channels, SystemTime::now(), 48000, client).unwrap()
    }

    fn venue(f: &Fixture, live: bool) -> Arc<Uplink> {
        venue_with(f, &f.pki.ca_pem, &f.pki.venue_pem, &f.pki.venue_key, live)
    }

    /// アップリンクを動かす。戻り値の送信側に true を送ると止まる
    fn connect(up: &Arc<Uplink>) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>, mpsc::Sender<Vec<u8>>) {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (live_tx, live_rx) = crate::uplink::live_queue();
        (stop_tx, tokio::spawn(up.clone().run(live_rx, stop_rx)), live_tx)
    }

    async fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
        reqwest::get(format!("http://{}{}", addr, path)).await.unwrap().json().await.unwrap()
    }

    async fn wait_for<F, Fut>(what: &str, f: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..100 {
            if f().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{} が起きませんでした", what);
    }

    fn transcript(text: &str) -> TranscriptResult {
        TranscriptResult::new(2, text.to_string(), false, None, SystemTime::now())
    }

    async fn history_texts(addr: SocketAddr) -> Vec<String> {
        get_json(addr, "/history?before=999999&limit=100").await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["x"].as_str().unwrap().to_string())
            .collect()
    }

    async fn segment_ids(addr: SocketAddr) -> Vec<u64> {
        get_json(addr, "/segments").await["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["i"].as_u64().unwrap())
            .collect()
    }

    /// SSE を読んで `until` を満たすまでのテキストを返す
    async fn read_sse(addr: SocketAddr, until: &str) -> String {
        let resp = reqwest::get(format!("http://{}/stream", addr)).await.unwrap();
        let mut body = resp.bytes_stream();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !text.contains(until) {
            match tokio::time::timeout_at(deadline, body.next()).await {
                Ok(Some(Ok(chunk))) => text.push_str(std::str::from_utf8(&chunk).unwrap()),
                _ => panic!("SSE に {} が来ません:\n{}", until, text),
            }
        }
        text
    }

    #[tokio::test]
    async fn events_and_segments_reach_viewers_and_resume_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let rec = tempfile::tempdir().unwrap();
        let f = start_relay(dir.path(), false).await;
        let addr = f.http;

        // 会場がつながる前は「切断中(最終受信なし)」
        let text = read_sse(addr, "event: up").await;
        assert!(text.contains(r#"{"on":false}"#), "{}", text);

        let up = venue(&f, false);
        // 区間録音はストリームのフロー制御の窓をまたぐ大きさにする
        let audio: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let path = rec.path().join("seg1.aac");
        std::fs::write(&path, &audio).unwrap();
        // つながる前に積んだ分も送られる
        up.push_transcript(&transcript("一報"), Some(1));
        up.push_segment(&SegmentInfo { id: 1, channel: 2, start_unix_ms: 1_000, duration_ms: 500, path });
        let (stop, task, _live) = connect(&up);
        let mut second = transcript("続報");
        second.audio_end_seconds = Some(3.25);
        up.push_transcript(&second, Some(1));

        wait_for("文字起こしの到着", || async { history_texts(addr).await.len() == 2 }).await;
        wait_for("区間録音の到着", || async { segment_ids(addr).await == vec![1] }).await;
        let got = reqwest::get(format!("http://{}/audio/1", addr)).await.unwrap().bytes().await.unwrap();
        assert_eq!(got.as_ref(), &audio[..]);
        // 会場との往復・未送信数・時計のずれが届き、心拍も来る
        wait_for("回線品質の配信", || async { read_sse(addr, "event: hb").await.contains(r#""rtt":"#) }).await;
        let text = read_sse(addr, "event: hb").await;
        assert!(text.contains(r#"{"on":true,"rtt":"#), "{}", text);
        assert!(text.contains(r#""q":0"#) && text.contains(r#""vo":"#), "{}", text);
        assert!(text.contains(r#"data: {"ms":"#), "{}", text);
        // 会場側にも測った結果が返る
        wait_for("会場の回線品質", || async { up.status().1.contains("ms") }).await;
        assert_eq!(up.status().0, crate::tui_state::Health::Good, "{:?}", up.status());
        assert!(text.contains(r#""channels":[{"id":2,"name":"本部"}]"#), "{}", text);
        // 文字起こしは区間 ID を保ったまま届く
        assert!(text.contains(r#""x":"一報","g":1"#), "{}", text);
        // 遅延の表示用に、発話の終わり(中継サーバの anchor に付け替え)と会場が受け取った時刻が載る
        assert!(text.contains(r#""x":"続報","g":1,"e":"#) && text.contains(r#","r":"#), "{}", text);
        assert_eq!(up.login_path().as_deref(), Some("/"));

        // 止めると閲覧画面には切断(最終受信時刻つき)が出る
        stop.send(true).unwrap();
        task.await.unwrap();
        wait_for("切断の配信", || async { read_sse(addr, "event: up").await.contains(r#""on":false,"t":"#) }).await;

        // 同じ起動でつなぎ直すと続きだけが届く(重複しない)
        up.push_transcript(&transcript("三報"), None);
        let (stop, task, _live) = connect(&up);
        wait_for("つなぎ直し後の到着", || async { history_texts(addr).await.len() == 3 }).await;
        assert_eq!(history_texts(addr).await, vec!["一報", "続報", "三報"]);
        assert_eq!(segment_ids(addr).await, vec![1], "区間録音も送り直さない");
        stop.send(true).unwrap();
        task.await.unwrap();

        // 会場を再起動すると、区間 ID は前の起動と重ならない範囲に振り直される
        let up2 = venue(&f, false);
        let path = rec.path().join("seg2.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        up2.push_transcript(&transcript("再起動後"), Some(1));
        up2.push_segment(&SegmentInfo { id: 1, channel: 2, start_unix_ms: 2_000, duration_ms: 100, path });
        let (stop, task, _live) = connect(&up2);
        wait_for("再起動後の区間録音", || async { segment_ids(addr).await == vec![1, 2] }).await;
        assert_eq!(history_texts(addr).await.len(), 4);
        let resp = reqwest::get(format!("http://{}/audio/2", addr)).await.unwrap();
        assert_eq!(resp.headers()["content-type"], "audio/wav");
        assert!(read_sse(addr, "再起動後").await.contains(r#""x":"再起動後","g":2"#));
        stop.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn certificates_from_another_ca_are_rejected_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let f = start_relay(dir.path(), false).await;
        let other = crate::pki::issue(&["127.0.0.1".into()], 30).unwrap();

        // 別の CA が署名した会場の証明書 → 中継サーバが拒否
        let intruder = venue_with(&f, &f.pki.ca_pem, &other.venue_pem, &other.venue_key, false);
        // 別の CA の中継サーバだと思っている会場 → 会場が中継サーバを信用しない
        let misled = venue_with(&f, &other.ca_pem, &f.pki.venue_pem, &f.pki.venue_key, false);
        let (s1, t1, _l1) = connect(&intruder);
        let (s2, t2, _l2) = connect(&misled);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(intruder.login_path().is_none(), "知らない証明書の会場はつながらない");
        assert!(misled.login_path().is_none(), "知らない中継サーバにはつながらない");
        assert_eq!(intruder.status().0, crate::tui_state::Health::Bad, "{:?}", intruder.status());
        let text = read_sse(f.http, "event: up").await;
        assert!(text.contains(r#"{"on":false}"#), "{}", text);
        for (s, t) in [(s1, t1), (s2, t2)] {
            s.send(true).unwrap();
            t.await.unwrap();
        }

        // 正しい証明書ならつながる
        let up = venue(&f, false);
        let (s, t, _l) = connect(&up);
        wait_for("正しい会場の接続", || async { up.login_path().is_some() }).await;
        s.send(true).unwrap();
        t.await.unwrap();
    }

    #[tokio::test]
    async fn live_audio_flows_only_while_someone_listens() {
        let dir = tempfile::tempdir().unwrap();
        let f = start_relay(dir.path(), true).await;
        let up = venue(&f, true);
        let (stop, task, live_out) = connect(&up);
        let (frame_tx, frame_rx) = mpsc::channel(256);
        tokio::spawn(up.clone().run_live_encoder(frame_rx, live_out));
        wait_for("会場の接続", || async { up.login_path().is_some() }).await;
        assert!(!up.live_active_flag().load(Ordering::Relaxed), "誰も聞いていなければ作らない");

        // 入力側の代わりに 1024 サンプルずつ流し続ける
        let active = up.live_active_flag();
        let feeder = tokio::spawn(async move {
            let mut t = 0usize;
            loop {
                if active.load(Ordering::Relaxed) {
                    let samples: Vec<i16> =
                        (0..1024).map(|i| (((t + i) as f32 * 0.06).sin() * 8000.0) as i16).collect();
                    t += 1024;
                    let at_ms = unix_ms(SystemTime::now());
                    let _ = frame_tx.try_send(LiveFrame { channels: vec![(2, samples), (3, vec![0; 1024])], at_ms });
                }
                tokio::time::sleep(Duration::from_millis(21)).await;
            }
        });

        let resp = reqwest::get(format!("http://{}/live?c=2&prefill=0", f.http)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let mut body = resp.bytes_stream();
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 2000 {
            match tokio::time::timeout_at(deadline, body.next()).await {
                Ok(Some(Ok(chunk))) => got.extend_from_slice(&chunk),
                _ => panic!("ライブ音声が届きません ({} bytes)", got.len()),
            }
        }
        assert_eq!(got[0], 0xFF, "ADTS で届く");
        let pcm = crate::aac::tests::decode(&got);
        let rms = (pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
        assert!(rms > 500.0, "会場の音が聞こえる rms={}", rms);
        assert!(up.live_active_flag().load(Ordering::Relaxed));
        // 会場で録ってから中継サーバに届くまでの遅延が閲覧画面に届く(手元なので 1 秒もかからない)
        wait_for("ライブの遅延の配信", || async { read_sse(f.http, "event: hb").await.contains(r#""ld":"#) }).await;
        let text = read_sse(f.http, "event: hb").await;
        let ld: u64 = text.split(r#""ld":"#).nth(1).unwrap().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
        assert!(ld < 1000, "ld={}", ld);

        // 聞く人がいなくなれば会場も送るのをやめる
        drop(body);
        wait_for("ライブ停止", || async { !up.live_active_flag().load(Ordering::Relaxed) }).await;
        feeder.abort();
        stop.send(true).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn lost_live_frames_are_filled_and_late_ones_dropped() {
        let hub = LiveHub::new(proto::LIVE_RATE, 16000, AacProfile::He, vec![2]);
        let mut rx = hub.subscribe(&[2]).unwrap();
        let mut enc = opus::Encoder::new(48000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_inband_fec(true).unwrap();
        enc.set_packet_loss_perc(10).unwrap();
        let mut out = vec![0u8; 1500];
        let frames: Vec<Vec<u8>> = (0..12)
            .map(|n| {
                let pcm: Vec<i16> = (0..960).map(|i| (((n * 960 + i) as f32 * 0.05).sin() * 8000.0) as i16).collect();
                let len = enc.encode(&pcm, &mut out).unwrap();
                proto::live_frame(n as u32, 0, &[(2, out[..len].to_vec())])
            })
            .collect();

        let mut lr = LiveReceiver::default();
        // 0, 1 → 3(2 が欠けた: FEC で 1 フレーム)→ 6(4, 5 が欠けた: 1 つ補間 + 1 つ FEC)
        // → 5(遅れて届いた: 捨てる)→ 20(13 フレーム欠け: 埋めない)
        for i in [0, 1, 3, 6, 5] {
            lr.on_datagram(&frames[i], &hub);
        }
        assert_eq!(lr.concealed, 3);
        lr.on_datagram(&proto::live_frame(20, 0, &[]), &hub);
        assert_eq!(lr.concealed, 3, "長い欠けは埋めない");
        // 届いた 4 フレーム + 埋めた 3 フレーム = 140 ms 分が LiveHub に入っている
        let mut bytes = 0;
        while let Ok(b) = rx.try_recv() {
            bytes += b.len();
        }
        assert!(bytes > 0);
    }
}
