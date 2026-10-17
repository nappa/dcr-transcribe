//! リモート閲覧用の HTTP/SSE サーバ
//!
//! `axum` で軽量サーバを localhost に立て、Cloudflare Tunnel (`cloudflared`) で公開する想定。
//!
//! # エンドポイント
//!
//! - `GET /` - HTML ページ(モバイル対応 SSE クライアント)
//! - `GET /stream` - Server-Sent Events
//!     - 接続時に `event: hello` で anchor 時刻とチャンネル一覧を送信
//!     - 続いて `event: msg` で履歴(直近 N 件、または `Last-Event-ID` 以降)を流す
//!     - 以降、新規イベントを `event: msg` で順次配信
//!     - 発話区間の録音が閉じるたびに `event: seg` を配信(ID なし、再接続時は `/segments` で回収)
//! - `GET /history` - 過去の文字起こし
//! - `GET /segments?after=<id>` - 録音区間の一覧(`after` より新しいもの)
//! - `GET /live?c=<id>,<id>` - 選んだチャンネルをミックスしたライブ音声(AAC/ADTS の連続ストリーム)
//! - `GET /audio/{id}` - 録音区間の AAC / WAV(Range リクエスト対応、iOS Safari の再生に必須)
//! - `GET /healthz` - "ok" 返却
//! - `POST /verify` - Turnstile のトークンを検証してセッションクッキーを発行
//! - `GET /session` - セッションが有効なら 204(期限切れを画面側で検知するため)
//!
//! # アクセス制限 (`crate::access`)
//!
//! - `/healthz` 以外は国の制限(`CF-IPCountry`)を通ったものだけ
//! - Turnstile が有効なら、`/` はセッションが無いと確認画面を返し、データ系はセッション必須
//!
//! # 細い回線向けの転送量対策
//!
//! - HTML は起動時に brotli(最高圧縮)で 1 度だけ圧縮し、ETag で 304 を返す
//! - `/history` `/segments` の JSON も、ある程度大きければ brotli で返す
//! - チャンネルの状態 (`event: st`) は変化したときだけ、短い配列で送る
//!
//! # 多クライアント・スロークライアント耐性
//!
//! - `tokio::sync::broadcast` ベース: 各クライアントは独立した Receiver
//! - 1 クライアントが TCP 書き込みで詰まっても他のクライアントには波及しない
//! - スロークライアントが broadcast capacity (256) ぶん遅れたら `Lagged` 検知 → 即切断
//! - クライアントは自動再接続し、`Last-Event-ID` で欠損分のみ history から再送
//!
//! # 回線断耐性 (モバイル前提)
//!
//! - 各イベントに単調増加 ID (`AtomicU64`) を付与
//! - SSE response の `Event::id()` で送信 → ブラウザが `lastEventId` を保持
//! - 再接続時に `Last-Event-ID` ヘッダから欠損分のみ送信(重複なし)
//! - history を超えて遅れた場合は現在の history 全体を送信(graceful fallback)
//!
//! # 既存パイプラインからの隔離
//!
//! - `RemoteState::push()` は同期で短時間 (Mutex+broadcast::send) のみ
//! - 障害時(broadcast 容量溢れ、クライアント切断)もログ警告のみ
//! - サーバタスクが落ちても他に波及しない

use crate::access::AccessControl;
use crate::config::{ChannelConfig, RemoteConfig};
use crate::live::LiveHub;
use crate::segment_recorder::SegmentInfo;
use crate::types::TranscriptResult;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::middleware::{self, Next};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine as _;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::broadcast;

const HTML_PAGE: &str = include_str!("../static/index.html");
const GATE_PAGE: &str = include_str!("../static/gate.html");
/// broadcast バッファ容量。スロークライアントが本数ぶん遅れた時点で Lagged として切断される。
/// 小さいほどスロークライアントを早く切るが、瞬間的なバースト時の誤切断も増える。
const BROADCAST_CAPACITY: usize = 256;
/// メモリに保持する録音区間の最大数。古いものは一覧から消える(ファイルは残る)
const MAX_SEGMENTS: usize = 1000;
/// これより小さい JSON は圧縮しない(ヘッダのほうが大きくなる)
const COMPRESS_MIN_BYTES: usize = 512;
/// 状態の音量は、受信中はこの dB 以上動いたら送る
const STATUS_LEVEL_STEP_RECEIVING: i8 = 6;
/// 受信していないときの音量はこの dB 以上動いたときだけ送る(雑音の揺れで送らない)
const STATUS_LEVEL_STEP_IDLE: i8 = 12;

/// SSE で配信する1イベント(転送量最小化のため短いキー)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireEvent {
    /// channel id
    c: usize,
    /// timestamp_seconds (anchor からの経過秒)
    s: f64,
    /// text
    x: String,
    /// partial フラグ。false なら省略
    #[serde(default, skip_serializing_if = "is_false")]
    p: bool,
    /// 録音区間 ID。区間録音が無効なら省略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g: Option<u64>,
}

/// `event: seg` / `/segments` で配信する録音区間
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct WireSegment {
    /// segment id
    i: u64,
    /// channel id
    c: usize,
    /// 開始時刻(UNIX ミリ秒)
    t: u64,
    /// 長さ(ミリ秒)
    d: u64,
}

/// 閲覧画面に出すチャンネルの状態(`event: st`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelStatus {
    pub channel: usize,
    /// 音量 (dB)
    pub level_db: f32,
    /// 受信中(音量が閾値を超えている)
    pub receiving: bool,
    /// Transcribe 接続。0=未接続 1=接続中 2=エラー
    pub transcribe: u8,
}

/// 配信形式 `[c, l, r, x]`(l は -60〜0 dB の整数)
type WireStatus = (usize, i8, u8, u8);

/// チャンネル状態の差分配信
struct StatusStore {
    /// 最後に送った状態
    last: Mutex<HashMap<usize, WireStatus>>,
    broadcaster: broadcast::Sender<Arc<str>>,
}

impl StatusStore {
    fn new() -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self {
            last: Mutex::new(HashMap::new()),
            broadcaster: tx,
        }
    }

    /// 全チャンネルの現状(接続直後・取りこぼし時の送り直し用)。まだ何も無ければ None
    fn snapshot(&self) -> Option<String> {
        let last = self.last.lock().unwrap();
        if last.is_empty() {
            return None;
        }
        let mut all: Vec<WireStatus> = last.values().copied().collect();
        all.sort_unstable();
        serde_json::to_string(&all).ok()
    }
}

/// 前回送った状態から意味のある変化があったか
fn status_changed(prev: Option<&WireStatus>, next: &WireStatus) -> bool {
    let Some(prev) = prev else { return true };
    if prev.2 != next.2 || prev.3 != next.3 {
        return true;
    }
    let step = if next.2 == 1 {
        STATUS_LEVEL_STEP_RECEIVING
    } else {
        STATUS_LEVEL_STEP_IDLE
    };
    (prev.1 - next.1).abs() >= step
}

/// 録音区間の一覧と配信
struct SegmentStore {
    enabled: bool,
    items: Mutex<VecDeque<(WireSegment, std::path::PathBuf)>>,
    broadcaster: broadcast::Sender<WireSegment>,
}

impl SegmentStore {
    fn new(enabled: bool) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            enabled,
            items: Mutex::new(VecDeque::new()),
            broadcaster: tx,
        }
    }

    /// `after` より新しい区間を古い順に返す。`limit` があれば新しい側の `limit` 件だけ
    fn after(&self, after: u64, limit: Option<usize>) -> Vec<WireSegment> {
        let items = self.items.lock().unwrap();
        let matching: Vec<WireSegment> =
            items.iter().filter(|(w, _)| w.i > after).map(|(w, _)| w.clone()).collect();
        let start = limit.map_or(0, |n| matching.len().saturating_sub(n));
        matching[start..].to_vec()
    }

    fn path(&self, id: u64) -> Option<std::path::PathBuf> {
        let items = self.items.lock().unwrap();
        items.iter().find(|(w, _)| w.i == id).map(|(_, p)| p.clone())
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// 内部ストレージ用: 単調増加 ID 付き
#[derive(Debug, Clone)]
struct StoredEvent {
    id: u64,
    event: WireEvent,
}

#[derive(Debug, Clone, Serialize)]
struct ChannelInfo {
    id: usize,
    name: String,
}

/// `event: hello` で送信される初期ペイロード
#[derive(Debug, Clone, Serialize)]
struct Hello {
    /// anchor 時刻(ISO 8601)。クライアントは `s` と組み合わせて壁時計時刻を計算
    anchor: String,
    channels: Vec<ChannelInfo>,
    /// 区間録音が有効か(クライアントは true のとき `/segments` を取りに行く)
    #[serde(skip_serializing_if = "is_false")]
    seg: bool,
    /// ライブ再生が使えるか
    #[serde(skip_serializing_if = "is_false")]
    live: bool,
}

#[derive(Deserialize)]
struct StreamQuery {
    #[serde(default)]
    t: Option<String>,
    /// 直近の N 件のみを送信(Last-Event-ID が無いときのみ有効)。
    /// モバイルが長時間 hidden の後に再接続するとき、欠損を全部辿らずに最新だけ即取得するため。
    #[serde(default)]
    head: Option<usize>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    t: Option<String>,
    /// この ID より小さいイベントを返す
    before: u64,
    /// 最大件数(サーバ側で history_size を上限にクランプ)
    #[serde(default = "default_history_limit")]
    limit: usize,
}

fn default_history_limit() -> usize {
    50
}

#[derive(Deserialize)]
struct SegmentsQuery {
    #[serde(default)]
    t: Option<String>,
    /// この ID より新しい区間を返す
    #[serde(default)]
    after: u64,
    /// 新しい側から最大何件返すか(初回読み込みで全件取らないため)
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct LiveQuery {
    #[serde(default)]
    t: Option<String>,
    /// 0 なら接続直後の無音(プリフィル)を送らない
    #[serde(default)]
    prefill: Option<u8>,
    /// カンマ区切りのチャンネル ID
    #[serde(default)]
    c: String,
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    t: Option<String>,
}

/// `subscribe` への mode 指定
#[derive(Debug, Clone, Copy)]
enum SubscribeMode {
    /// 履歴を全件
    All,
    /// 指定 ID より新しいもののみ(`Last-Event-ID` 由来)
    Since(u64),
    /// 直近 N 件のみ(ファストロード用)
    Head(usize),
}

/// リモートサーバの共有状態
pub struct RemoteState {
    history: Mutex<VecDeque<StoredEvent>>,
    last_partial: Mutex<HashMap<usize, Instant>>,
    broadcaster: broadcast::Sender<StoredEvent>,
    next_id: AtomicU64,
    history_size: usize,
    include_partials: bool,
    partial_throttle: Duration,
    auth_token: String,
    basic_auth_user: String,
    basic_auth_pass: String,
    anchor: String,
    channels: Vec<ChannelInfo>,
    segments: SegmentStore,
    live: Option<Arc<LiveHub>>,
    status: StatusStore,
    index: Page,
    access: AccessControl,
    /// Turnstile の確認画面(Turnstile 無効なら None)
    gate: Option<Page>,
}

/// 圧縮済みの HTML
struct Page {
    html: String,
    br: Vec<u8>,
    etag: String,
}

impl Page {
    fn new(html: String) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        html.hash(&mut h);
        Self {
            br: brotli_compress(html.as_bytes(), 11),
            etag: format!("\"{:016x}\"", h.finish()),
            html,
        }
    }

    fn index() -> Self {
        Self::new(HTML_PAGE.to_string())
    }

    fn gate(access: &AccessControl) -> Option<Self> {
        access
            .turnstile()
            .map(|ts| Self::new(GATE_PAGE.replace("__SITE_KEY__", &ts.site_key)))
    }

    /// `cache_control` を付けて返す。ETag が一致すれば 304、brotli 対応なら圧縮して返す
    fn respond(&self, headers: &HeaderMap, cache_control: &'static str) -> Response {
        let etag = self.etag.as_str();
        let common = [
            (header::CACHE_CONTROL, cache_control),
            (header::ETAG, etag),
            (header::VARY, "Accept-Encoding"),
        ];
        let not_modified = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
        if not_modified {
            return (StatusCode::NOT_MODIFIED, common).into_response();
        }
        let ctype = (header::CONTENT_TYPE, "text/html; charset=utf-8");
        if accepts_brotli(headers) {
            (StatusCode::OK, common, [ctype, (header::CONTENT_ENCODING, "br")], self.br.clone())
                .into_response()
        } else {
            (StatusCode::OK, common, [ctype], self.html.clone()).into_response()
        }
    }
}

fn brotli_compress(data: &[u8], quality: u32) -> Vec<u8> {
    use std::io::Write;
    let mut out = Vec::with_capacity(data.len() / 3);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, quality, 22);
        // Vec への書き込みは失敗しない
        let _ = w.write_all(data);
    }
    out
}

fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| e.trim().split(';').next() == Some("br")))
}

/// JSON を返す。クライアントが brotli を受け付け、ある程度大きければ圧縮する
fn json_response(body: String, headers: &HeaderMap) -> Response {
    let ctype = (header::CONTENT_TYPE, "application/json");
    let cache = (header::CACHE_CONTROL, "no-store");
    let vary = (header::VARY, "Accept-Encoding");
    if body.len() >= COMPRESS_MIN_BYTES && accepts_brotli(headers) {
        let br = brotli_compress(body.as_bytes(), 5);
        return (
            StatusCode::OK,
            [ctype, cache, vary, (header::CONTENT_ENCODING, "br")],
            br,
        )
            .into_response();
    }
    (StatusCode::OK, [ctype, cache, vary], body).into_response()
}

impl RemoteState {
    /// 文字起こし結果を投入(同期、ノンブロッキング)
    #[allow(dead_code)] // バイナリ側は push_with_segment を使う。テストとライブラリ利用向け
    pub fn push(&self, result: &TranscriptResult) {
        self.push_with_segment(result, None);
    }

    /// 録音区間 ID 付きで文字起こし結果を投入
    pub fn push_with_segment(&self, result: &TranscriptResult, segment: Option<u64>) {
        if result.is_partial {
            if !self.include_partials {
                return;
            }
            // チャンネルごとに throttle
            let mut last = self.last_partial.lock().unwrap();
            let now = Instant::now();
            if let Some(prev) = last.get(&result.channel) {
                if now.duration_since(*prev) < self.partial_throttle {
                    return;
                }
            }
            last.insert(result.channel, now);
        }

        let event = WireEvent {
            c: result.channel,
            s: result.timestamp_seconds,
            x: result.text.clone(),
            p: result.is_partial,
            g: segment,
        };

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stored = StoredEvent { id, event };

        // history に追加 + broadcast(同一 Mutex 配下で subscribe との一貫性を保つ)
        let mut hist = self.history.lock().unwrap();
        hist.push_back(stored.clone());
        while hist.len() > self.history_size {
            hist.pop_front();
        }
        let _ = self.broadcaster.send(stored); // 受信者ゼロでもエラーにしない
    }

    /// 録音を終えた区間を登録して配信(同期、ノンブロッキング)
    pub fn push_segment(&self, info: &SegmentInfo) {
        let wire = WireSegment {
            i: info.id,
            c: info.channel,
            t: info.start_unix_ms,
            d: info.duration_ms,
        };
        let mut items = self.segments.items.lock().unwrap();
        items.push_back((wire.clone(), info.path.clone()));
        while items.len() > MAX_SEGMENTS {
            items.pop_front();
        }
        let _ = self.segments.broadcaster.send(wire);
    }

    /// チャンネルの状態を更新し、変化があったチャンネルだけ配信する(同期、ノンブロッキング)
    pub fn push_status(&self, items: &[ChannelStatus]) {
        let mut last = self.status.last.lock().unwrap();
        let mut changed: Vec<WireStatus> = Vec::new();
        for st in items {
            let wire = (
                st.channel,
                st.level_db.clamp(-60.0, 0.0).round() as i8,
                st.receiving as u8,
                st.transcribe,
            );
            if status_changed(last.get(&st.channel), &wire) {
                last.insert(st.channel, wire);
                changed.push(wire);
            }
        }
        if changed.is_empty() {
            return;
        }
        if let Ok(s) = serde_json::to_string(&changed) {
            let _ = self.status.broadcaster.send(Arc::from(s));
        }
    }

    /// SSE 接続用に「履歴のスナップショット」と「新規イベント受信用 Receiver」を atomic に取得。
    fn subscribe(
        &self,
        mode: SubscribeMode,
    ) -> (Vec<StoredEvent>, broadcast::Receiver<StoredEvent>) {
        let hist = self.history.lock().unwrap();
        let rx = self.broadcaster.subscribe();
        let snapshot: Vec<StoredEvent> = match mode {
            SubscribeMode::All => hist.iter().cloned().collect(),
            SubscribeMode::Since(last) => hist.iter().filter(|e| e.id > last).cloned().collect(),
            SubscribeMode::Head(n) => {
                let start = hist.len().saturating_sub(n);
                hist.iter().skip(start).cloned().collect()
            }
        };
        (snapshot, rx)
    }

    /// 過去にさかのぼる用: `before` より小さい ID のイベントを最新側から最大 `limit` 件返す。
    fn history_before(&self, before: u64, limit: usize) -> Vec<StoredEvent> {
        let hist = self.history.lock().unwrap();
        let matching: Vec<StoredEvent> = hist.iter().filter(|e| e.id < before).cloned().collect();
        let start = matching.len().saturating_sub(limit);
        matching[start..].to_vec()
    }

    fn check_token(&self, token: Option<&str>) -> bool {
        if self.auth_token.is_empty() {
            return true;
        }
        token == Some(self.auth_token.as_str())
    }

    /// HTTP Basic 認証チェック。
    ///
    /// `basic_auth_user` が空なら無効化(常に true)。
    /// それ以外は `Authorization: Basic <base64(user:pass)>` ヘッダを要求。
    fn check_basic_auth(&self, headers: &HeaderMap) -> bool {
        if self.basic_auth_user.is_empty() {
            return true;
        }
        let raw = match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            Some(s) => s,
            None => return false,
        };
        let encoded = match raw.strip_prefix("Basic ") {
            Some(s) => s.trim(),
            None => return false,
        };
        let decoded = match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(d) => d,
            Err(_) => return false,
        };
        let creds = match std::str::from_utf8(&decoded) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let (user, pass) = match creds.split_once(':') {
            Some(p) => p,
            None => return false,
        };
        user == self.basic_auth_user && pass == self.basic_auth_pass
    }
}

/// 401 Unauthorized + WWW-Authenticate(ブラウザに Basic 認証プロンプトを出させる)
fn unauthorized_basic() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"dcr-transcribe\"")],
        "Authentication required",
    )
        .into_response()
}

/// HTTP サーバを起動して `RemoteState` の Arc を返す
pub async fn start(
    config: RemoteConfig,
    channels_config: &[ChannelConfig],
    start_time: SystemTime,
    segments_enabled: bool,
    live: Option<Arc<LiveHub>>,
) -> Result<Arc<RemoteState>> {
    let anchor = chrono::DateTime::<chrono::Utc>::from(start_time).to_rfc3339();
    let channels: Vec<ChannelInfo> = channels_config
        .iter()
        .filter(|c| c.enabled)
        .map(|c| ChannelInfo {
            id: c.id,
            name: c.name.clone(),
        })
        .collect();

    let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
    let access = AccessControl::new(
        &config.allowed_countries,
        &config.turnstile_site_key,
        &config.turnstile_secret_key,
        Duration::from_secs(config.session_hours * 3600),
    );
    let access_desc = access.describe();

    let state = Arc::new(RemoteState {
        history: Mutex::new(VecDeque::with_capacity(config.history_size)),
        last_partial: Mutex::new(HashMap::new()),
        broadcaster: tx,
        next_id: AtomicU64::new(1),
        history_size: config.history_size,
        include_partials: config.include_partials,
        partial_throttle: Duration::from_millis(config.partial_throttle_ms),
        auth_token: config.auth_token.clone(),
        basic_auth_user: config.basic_auth_user.clone(),
        basic_auth_pass: config.basic_auth_pass.clone(),
        anchor,
        channels,
        segments: SegmentStore::new(segments_enabled),
        live,
        status: StatusStore::new(),
        index: Page::index(),
        gate: Page::gate(&access),
        access,
    });

    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("Remote サーバの bind に失敗: {}", config.listen))?;

    let actual_addr = listener.local_addr().ok();
    let auth_desc = match (
        !config.auth_token.is_empty(),
        !config.basic_auth_user.is_empty(),
    ) {
        (false, false) => "off".to_string(),
        (true, false) => "token".to_string(),
        (false, true) => "basic".to_string(),
        (true, true) => "basic+token".to_string(),
    };
    log::info!(
        "Remote サーバ起動: http://{} (auth={}, {}, partials={})",
        actual_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| config.listen.clone()),
        auth_desc,
        access_desc,
        config.include_partials
    );

    // 小さな書き込み(SSE の 1 イベント、ライブの AAC 1 フレーム)を溜めずにすぐ送る(Nagle 無効)
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp: &mut tokio::net::TcpStream| {
        if let Err(e) = tcp.set_nodelay(true) {
            log::debug!("Remote: TCP_NODELAY の設定に失敗: {}", e);
        }
    });
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("Remote サーバが終了: {}", e);
        }
    });

    Ok(state)
}

fn router(state: Arc<RemoteState>) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/stream", get(serve_stream))
        .route("/history", get(serve_history))
        .route("/segments", get(serve_segments))
        .route("/audio/{id}", get(serve_audio))
        .route("/live", get(serve_live))
        .route("/verify", post(serve_verify))
        .route("/session", get(serve_session))
        .route("/healthz", get(serve_health))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// 全リクエスト共通の入口: 国の制限と、データ系の bot 確認セッション
async fn guard(State(state): State<Arc<RemoteState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/healthz" {
        return next.run(req).await;
    }
    if !state.access.country_allowed(req.headers()) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    // `/` はセッションが無ければ確認画面を返す。`/verify` はセッションを作るところ
    let needs_session = !matches!(path, "/" | "/verify");
    if needs_session && !state.access.session_valid(req.headers()) {
        return (StatusCode::FORBIDDEN, "verification required").into_response();
    }
    next.run(req).await
}

// ─────────────────────────────────────────────────────────────────────────
// Handlers
// ─────────────────────────────────────────────────────────────────────────

async fn serve_index(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    // bot 確認がまだなら確認画面(キャッシュさせない)
    if let Some(gate) = &state.gate {
        if !state.access.session_valid(&headers) {
            return gate.respond(&headers, "no-store");
        }
    }
    // 毎回確認させるが、変わっていなければ 304 で本文を送らない
    state.index.respond(&headers, "private, no-cache")
}

#[derive(Deserialize)]
struct VerifyBody {
    token: String,
}

/// Turnstile のトークンを検証してセッションクッキーを発行する
async fn serve_verify(
    State(state): State<Arc<RemoteState>>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<VerifyBody>,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    let remote_ip = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok());
    match state.access.verify(&body.token, remote_ip).await {
        Ok(id) => (
            StatusCode::NO_CONTENT,
            [
                (header::SET_COOKIE, state.access.session_cookie(&id)),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
        )
            .into_response(),
        Err(e) => {
            log::warn!("Remote: bot 確認に失敗 (ip={:?}): {}", remote_ip, e);
            (StatusCode::FORBIDDEN, "verification failed").into_response()
        }
    }
}

/// セッションが有効なら 204(無効なら guard が 403 を返す)
async fn serve_session() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

/// /healthz: 認証なし(外部監視用)
async fn serve_health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn serve_stream(
    State(state): State<Arc<RemoteState>>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    if !state.check_token(q.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    // モード優先度: Last-Event-ID > head > 全件。
    //
    // この順序が重要: ?head=N でファストロード接続したあと、回線断による自動再接続では
    // EventSource が Last-Event-ID を付与する。?head は URL に残っているが Last-Event-ID
    // を優先することで、ファストロード後の再接続では通常の差分送信になり、欠損が再発しない。
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mode = match (last_event_id, q.head) {
        (Some(id), _) => SubscribeMode::Since(id),
        (None, Some(n)) => SubscribeMode::Head(n.min(state.history_size)),
        (None, None) => SubscribeMode::All,
    };

    let stream = build_stream(state, mode);
    Sse::new(stream)
        // モバイルプロキシで TCP がアイドル切断されないよう keep-alive コメントを定期送信
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

async fn serve_history(
    State(state): State<Arc<RemoteState>>,
    Query(q): Query<HistoryQuery>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    if !state.check_token(q.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    // history_size=0 の場合 clamp(1, 0) は panic するので分岐
    let limit = if state.history_size == 0 {
        0
    } else {
        q.limit.clamp(1, state.history_size)
    };
    let snapshot = state.history_before(q.before, limit);
    let items: Vec<serde_json::Value> = snapshot
        .iter()
        .map(|e| {
            // ID も含めて返す(SSE と違い meta の event-id は使えないので body に含める)
            serde_json::json!({
                "id": e.id,
                "c": e.event.c,
                "s": e.event.s,
                "x": e.event.x,
                "p": e.event.p,
            })
        })
        .collect();
    let body = serde_json::json!({ "items": items }).to_string();
    json_response(body, &headers)
}

async fn serve_segments(
    State(state): State<Arc<RemoteState>>,
    Query(q): Query<SegmentsQuery>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    if !state.check_token(q.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let body = serde_json::json!({ "items": state.segments.after(q.after, q.limit) }).to_string();
    json_response(body, &headers)
}

/// 録音区間のファイルを返す。一覧に登録済みの ID だけを受け付ける(任意パスは読ませない)
async fn serve_audio(
    State(state): State<Arc<RemoteState>>,
    Path(id): Path<u64>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    if !state.check_token(q.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(path) = state.segments.path(id) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(e) => {
            log::warn!("Remote: 区間 WAV の読み込みに失敗 {:?}: {}", path, e);
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    };
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("wav") => "audio/wav",
        _ => "audio/aac",
    };
    audio_response(data, content_type, headers.get(header::RANGE).and_then(|v| v.to_str().ok()))
}

/// 選んだチャンネルのミックスを AAC (ADTS) で流し続ける。
///
/// 長さの決まらない 1 本のレスポンスなので、ブラウザは `<audio src>` でネットラジオと同じように再生する。
/// 遅れたクライアントは古いフレームを飛ばして追いつかせる(音は途切れるが遅延は溜めない)。
async fn serve_live(
    State(state): State<Arc<RemoteState>>,
    Query(q): Query<LiveQuery>,
    headers: HeaderMap,
) -> Response {
    if !state.check_basic_auth(&headers) {
        return unauthorized_basic();
    }
    if !state.check_token(q.t.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let Some(hub) = &state.live else {
        return (StatusCode::NOT_FOUND, "live disabled").into_response();
    };
    let channels: Option<Vec<usize>> = q.c.split(',').map(|s| s.trim().parse().ok()).collect();
    let mut rx = match channels.map(|c| hub.subscribe(&c)) {
        Some(Ok(rx)) => rx,
        Some(Err(e)) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        None => return (StatusCode::BAD_REQUEST, "invalid channel list").into_response(),
    };
    let prefill = (q.prefill != Some(0)).then(|| hub.prefill());
    let stream = async_stream::stream! {
        // ブラウザがすぐ再生を始められるよう、先に無音をまとめて送る(LiveHub::prefill)
        if let Some(p) = prefill {
            yield Ok::<_, Infallible>(p);
        }
        loop {
            match rx.recv().await {
                Ok(bytes) => yield Ok::<_, Infallible>(bytes),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::debug!("Remote live: 遅れたクライアントのフレームを {} 個飛ばした", n);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "audio/aac"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// 音声ファイル本体を Range 指定に従って返す。
///
/// iOS Safari は `<audio>` 再生時に必ず Range リクエストを送り、206 が返らないと再生しない。
/// 区間ファイルは数 MB 程度なので、全体を読んでからスライスする。
fn audio_response(data: Vec<u8>, content_type: &'static str, range: Option<&str>) -> Response {
    let len = data.len() as u64;
    // 認証付きなので Cloudflare 等の共有キャッシュには載せない
    let cache = (header::CACHE_CONTROL, "private, max-age=86400");
    let accept = (header::ACCEPT_RANGES, "bytes");
    let ctype = (header::CONTENT_TYPE, content_type);

    let Some(range) = range else {
        return (StatusCode::OK, [ctype, accept, cache], data).into_response();
    };
    match parse_range(range, len) {
        Some((start, end)) => {
            let content_range = format!("bytes {}-{}/{}", start, end, len);
            let body = data[start as usize..=end as usize].to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                [ctype, accept, cache],
                [(header::CONTENT_RANGE, content_range)],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{}", len))],
        )
            .into_response(),
    }
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` を [start, end] (両端含む) に変換。
/// 複数範囲には対応しない(ブラウザの音声再生では使われない)。
fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { len - 1 } else { b.parse::<u64>().ok()?.min(len - 1) };
        (start, end)
    };
    (start <= end && start < len).then_some((start, end))
}

fn build_stream(
    state: Arc<RemoteState>,
    mode: SubscribeMode,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // hello より前に購読しておく(hello を受けたクライアントが /segments を取るまでの取りこぼし防止)
        let mut seg_rx = state.segments.broadcaster.subscribe();
        let mut status_rx = state.status.broadcaster.subscribe();

        // 1. hello イベント(anchor + チャンネル名)
        let hello = Hello {
            anchor: state.anchor.clone(),
            channels: state.channels.clone(),
            seg: state.segments.enabled,
            live: state.live.is_some(),
        };
        let hello_data = serde_json::to_string(&hello).unwrap_or_else(|_| "{}".into());
        yield Ok(Event::default().event("hello").data(hello_data));

        // 2. 履歴のスナップショット + Receiver を atomic に取得
        let (snapshot, mut rx) = state.subscribe(mode);

        // 3. 履歴を順次配信(各イベントに ID 付与)
        for stored in snapshot {
            if let Ok(s) = serde_json::to_string(&stored.event) {
                yield Ok(Event::default()
                    .event("msg")
                    .id(stored.id.to_string())
                    .data(s));
            }
        }

        // 3.5 チャンネルの現状
        if let Some(s) = state.status.snapshot() {
            yield Ok(Event::default().event("st").data(s));
        }

        // 4. ライブ配信
        loop {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(stored) => {
                        if let Ok(s) = serde_json::to_string(&stored.event) {
                            yield Ok(Event::default()
                                .event("msg")
                                .id(stored.id.to_string())
                                .data(s));
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // スロークライアントは即切断。
                        // ブラウザは Last-Event-ID で再接続して history から欠損分を回収する。
                        log::warn!(
                            "Remote SSE: スロークライアントを切断 ({} events behind)",
                            n
                        );
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                r = seg_rx.recv() => match r {
                    // 区間は ID を付けずに送る(lastEventId を文字起こし側の ID のまま保つため)
                    Ok(seg) => {
                        if let Ok(s) = serde_json::to_string(&seg) {
                            yield Ok(Event::default().event("seg").data(s));
                        }
                    }
                    // 区間は再接続時に /segments で回収できるので、文字起こしと同様に切断する
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                r = status_rx.recv() => match r {
                    Ok(s) => yield Ok(Event::default().event("st").data(s.to_string())),
                    // 状態は最新だけ分かればよいので、取りこぼしたら現状を送り直して続ける
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Some(s) = state.status.snapshot() {
                            yield Ok(Event::default().event("st").data(s));
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TranscriptResult;

    fn make_state(include_partials: bool, history_size: usize) -> Arc<RemoteState> {
        let (tx, _rx) = broadcast::channel(64);
        Arc::new(RemoteState {
            history: Mutex::new(VecDeque::with_capacity(history_size)),
            last_partial: Mutex::new(HashMap::new()),
            broadcaster: tx,
            next_id: AtomicU64::new(1),
            history_size,
            include_partials,
            partial_throttle: Duration::from_millis(100),
            auth_token: String::new(),
            basic_auth_user: String::new(),
            basic_auth_pass: String::new(),
            anchor: "2025-01-01T00:00:00Z".to_string(),
            channels: vec![],
            segments: SegmentStore::new(false),
            live: None,
            status: StatusStore::new(),
            index: Page::index(),
            access: AccessControl::disabled(),
            gate: None,
        })
    }

    fn basic_header(user: &str, pass: &str) -> HeaderMap {
        let cred = format!("{}:{}", user, pass);
        let encoded = base64::engine::general_purpose::STANDARD.encode(cred.as_bytes());
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Basic {}", encoded).parse().unwrap(),
        );
        h
    }

    fn make_result(channel: usize, text: &str, is_partial: bool) -> TranscriptResult {
        TranscriptResult::new(channel, text.to_string(), is_partial, None, SystemTime::now())
    }

    #[test]
    fn final_only_drops_partials() {
        let s = make_state(false, 10);
        s.push(&make_result(0, "final", false));
        s.push(&make_result(0, "partial", true));
        let hist = s.history.lock().unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].event.x, "final");
    }

    #[test]
    fn history_capped_at_size() {
        let s = make_state(false, 3);
        for i in 0..10 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        let hist = s.history.lock().unwrap();
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].event.x, "m7");
        assert_eq!(hist[2].event.x, "m9");
    }

    #[test]
    fn partial_throttle_per_channel() {
        let s = make_state(true, 100);
        s.push(&make_result(0, "p1", true));
        s.push(&make_result(0, "p2", true));
        s.push(&make_result(0, "p3", true));
        s.push(&make_result(1, "q1", true));
        let hist = s.history.lock().unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].event.x, "p1");
        assert_eq!(hist[1].event.x, "q1");
    }

    #[test]
    fn auth_token_validates() {
        let mut s = make_state(false, 10);
        Arc::get_mut(&mut s).unwrap().auth_token = "secret".to_string();
        assert!(s.check_token(Some("secret")));
        assert!(!s.check_token(Some("wrong")));
        assert!(!s.check_token(None));
    }

    #[test]
    fn auth_disabled_when_token_empty() {
        let s = make_state(false, 10);
        assert!(s.check_token(None));
        assert!(s.check_token(Some("anything")));
    }

    #[test]
    fn basic_auth_disabled_when_user_empty() {
        let s = make_state(false, 10);
        // ヘッダなしでも、設定が空なら true
        assert!(s.check_basic_auth(&HeaderMap::new()));
        // 適当なヘッダでも true
        assert!(s.check_basic_auth(&basic_header("anyone", "anything")));
    }

    #[test]
    fn basic_auth_accepts_correct_creds() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        assert!(s.check_basic_auth(&basic_header("alice", "secret")));
    }

    #[test]
    fn basic_auth_rejects_wrong_password() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        assert!(!s.check_basic_auth(&basic_header("alice", "wrong")));
    }

    #[test]
    fn basic_auth_rejects_wrong_user() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        assert!(!s.check_basic_auth(&basic_header("bob", "secret")));
    }

    #[test]
    fn basic_auth_rejects_missing_header() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        assert!(!s.check_basic_auth(&HeaderMap::new()));
    }

    #[test]
    fn basic_auth_rejects_malformed_header() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        let mut h = HeaderMap::new();
        // "Basic" prefix なし
        h.insert(header::AUTHORIZATION, "Bearer xyz".parse().unwrap());
        assert!(!s.check_basic_auth(&h));

        // base64 デコードできない
        let mut h2 = HeaderMap::new();
        h2.insert(header::AUTHORIZATION, "Basic !@#$%^".parse().unwrap());
        assert!(!s.check_basic_auth(&h2));

        // base64 はデコードできるが ":" が無い
        let mut h3 = HeaderMap::new();
        let no_colon = base64::engine::general_purpose::STANDARD.encode(b"justuser");
        h3.insert(
            header::AUTHORIZATION,
            format!("Basic {}", no_colon).parse().unwrap(),
        );
        assert!(!s.check_basic_auth(&h3));
    }

    #[test]
    fn basic_auth_handles_password_with_colon() {
        // パスワードにコロンが含まれる場合(RFC 7617)
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "pass:with:colons".to_string();
        assert!(s.check_basic_auth(&basic_header("alice", "pass:with:colons")));
    }

    // ─────────────────────────────────────────────────────────────────
    // HTTP 統合テスト(実サーバを 127.0.0.1:0 でバインド)
    // ─────────────────────────────────────────────────────────────────

    use std::net::SocketAddr;

    async fn boot(state: Arc<RemoteState>) -> SocketAddr {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    #[tokio::test]
    async fn http_healthz_no_auth_required() {
        let s = make_state(false, 10);
        let addr = boot(s).await;
        let resp = reqwest::get(format!("http://{}/healthz", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn http_healthz_bypasses_basic_auth() {
        // /healthz は外部監視用なので Basic 認証設定中でも 200 を返すべき
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        let addr = boot(s).await;

        let resp = reqwest::get(format!("http://{}/healthz", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn http_index_no_auth_returns_html() {
        let s = make_state(false, 10);
        let addr = boot(s).await;
        let resp = reqwest::get(format!("http://{}/", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ct.starts_with("text/html"));
        let body = resp.text().await.unwrap();
        assert!(body.contains("<!DOCTYPE html>"));
    }

    #[tokio::test]
    async fn http_index_with_basic_auth_required() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        let addr = boot(s).await;

        // 認証なし → 401 + WWW-Authenticate
        let resp = reqwest::get(format!("http://{}/", addr)).await.unwrap();
        assert_eq!(resp.status(), 401);
        let www = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .unwrap();
        assert!(www.to_str().unwrap().starts_with("Basic"));

        // 誤った creds → 401
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/", addr))
            .basic_auth("alice", Some("wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // 正しい creds → 200
        let resp = client
            .get(format!("http://{}/", addr))
            .basic_auth("alice", Some("secret"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn http_history_returns_json_with_ids() {
        let s = make_state(false, 10);
        for i in 0..5 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        let addr = boot(s).await;
        let resp = reqwest::get(format!("http://{}/history?before=999&limit=10", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let json: serde_json::Value = resp.json().await.unwrap();
        let items = json["items"].as_array().unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0]["id"], 1);
        assert_eq!(items[4]["id"], 5);
        assert_eq!(items[0]["x"], "m0");
        assert_eq!(items[4]["x"], "m4");
    }

    #[tokio::test]
    async fn http_history_token_auth() {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        m.auth_token = "supersecret".to_string();
        let addr = boot(s).await;

        // トークンなし → 401
        let resp = reqwest::get(format!("http://{}/history?before=999&limit=10", addr))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // 正しいトークン → 200
        let resp = reqwest::get(format!(
            "http://{}/history?before=999&limit=10&t=supersecret",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn http_history_with_zero_history_size_does_not_crash() {
        // バグ嫌疑: history_size が 0 のとき、serve_history の `clamp(1, history_size)` が
        // panic する可能性(clamp は min > max で panic する)。
        // これが起きると HTTP 接続が異常終了する。
        let s = make_state(false, 0);
        let addr = boot(s).await;
        let r = reqwest::get(format!("http://{}/history?before=999&limit=10", addr)).await;
        match r {
            Ok(resp) => {
                // 正常応答 or サーバ側で 500 を返すのは許容、接続が壊れなければ OK
                assert!(resp.status().as_u16() < 600);
            }
            Err(e) => {
                // 接続自体が失敗するならハンドラ panic の疑い濃厚
                panic!("HTTP request failed (handler panic suspected): {}", e);
            }
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // 並行 push/subscribe の race-free 性
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn concurrent_push_and_subscribe() {
        let s = make_state(false, 200);
        let mut handles = vec![];

        // 8 並列 pusher
        for i in 0..8 {
            let s_clone = s.clone();
            handles.push(tokio::spawn(async move {
                for j in 0..50 {
                    s_clone.push(&make_result(i, &format!("m{}-{}", i, j), false));
                    tokio::task::yield_now().await;
                }
            }));
        }

        // 4 並列 subscriber(snapshot 取得を繰り返す)
        for _ in 0..4 {
            let s_clone = s.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..30 {
                    let (_snap, _rx) = s_clone.subscribe(SubscribeMode::All);
                    tokio::task::yield_now().await;
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let hist = s.history.lock().unwrap();
        assert_eq!(hist.len(), 200, "history should be capped to history_size");
        // ID は厳密に単調増加でなければならない
        for i in 1..hist.len() {
            assert!(
                hist[i].id > hist[i - 1].id,
                "ID monotonicity violated at idx {}: {} <= {}",
                i,
                hist[i].id,
                hist[i - 1].id
            );
        }
        // ID の最終値 = 8 pushers × 50 events = 400
        assert_eq!(hist[hist.len() - 1].id, 400);
    }

    // ─────────────────────────────────────────────────────────────────
    // SSE wire format(実 HTTP 経由でテキスト解析)
    // ─────────────────────────────────────────────────────────────────

    /// 指定したアドレスの /stream を一定時間読んでテキストとして返すヘルパ
    async fn read_sse_text(
        addr: SocketAddr,
        last_event_id: Option<&str>,
        max_wait: Duration,
        until: impl Fn(&str) -> bool,
    ) -> String {
        use futures_util::StreamExt;
        use std::time::Instant;

        let mut req = reqwest::Client::new().get(format!("http://{}/stream", addr));
        if let Some(id) = last_event_id {
            req = req.header("Last-Event-ID", id);
        }
        let resp = req.send().await.expect("connect /stream");
        assert_eq!(resp.status(), 200);

        let mut byte_stream = resp.bytes_stream();
        let mut text = String::new();
        let deadline = Instant::now() + max_wait;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), byte_stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    text.push_str(std::str::from_utf8(&chunk).unwrap_or(""));
                    if until(&text) {
                        break;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => break,
                Err(_) => continue, // chunk timeout, keep waiting until deadline
            }
        }
        text
    }

    #[tokio::test]
    async fn http_sse_emits_hello_first_then_msg_with_id() {
        let s = make_state(false, 10);
        s.push(&make_result(0, "hello-text", false));
        let addr = boot(s).await;

        let text = read_sse_text(addr, None, Duration::from_secs(2), |t| {
            t.contains("event: msg")
        })
        .await;

        assert!(text.contains("event: hello"), "no hello event:\n{}", text);
        assert!(text.contains("event: msg"), "no msg event:\n{}", text);
        // SSE の id: フィールドは Last-Event-ID 連動のキー(ここでは id=1)
        assert!(text.contains("id: 1"), "msg should have `id: 1` line:\n{}", text);
        assert!(text.contains("hello-text"), "msg should contain pushed text:\n{}", text);

        // hello は msg の前
        let hello_pos = text.find("event: hello").unwrap();
        let msg_pos = text.find("event: msg").unwrap();
        assert!(hello_pos < msg_pos, "hello should come before msg");

        // hello の data は anchor + channels の JSON
        let after_hello = &text[hello_pos..msg_pos];
        assert!(after_hello.contains("anchor"));
        assert!(after_hello.contains("channels"));
    }

    #[tokio::test]
    async fn http_sse_partial_dropped_when_disabled() {
        let s = make_state(false, 10); // include_partials=false
        s.push(&make_result(0, "final-1", false));
        s.push(&make_result(0, "partial-1", true)); // disabled なので即捨てられる
        s.push(&make_result(0, "final-2", false));
        let addr = boot(s).await;

        let text = read_sse_text(addr, None, Duration::from_secs(1), |t| {
            t.contains("final-2")
        })
        .await;

        assert!(text.contains("final-1"));
        assert!(text.contains("final-2"));
        assert!(
            !text.contains("partial-1"),
            "partial should be filtered out, got:\n{}",
            text
        );
    }

    #[tokio::test]
    async fn http_sse_includes_partials_when_enabled() {
        let s = make_state(true, 10); // include_partials=true
        s.push(&make_result(0, "p-only", true));
        let addr = boot(s).await;

        let text = read_sse_text(addr, None, Duration::from_secs(1), |t| {
            t.contains("p-only")
        })
        .await;

        assert!(text.contains("p-only"));
        // p フラグが wire に出ること(p:true)
        assert!(text.contains("\"p\":true"), "partial flag missing:\n{}", text);
    }

    #[tokio::test]
    async fn http_sse_last_event_id_resumes_from_position() {
        let s = make_state(false, 100);
        for i in 1..=5 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        let addr = boot(s).await;

        // Last-Event-ID: 3 → m4, m5 のみが履歴から流れる
        let text = read_sse_text(addr, Some("3"), Duration::from_secs(1), |t| {
            t.contains("m5")
        })
        .await;

        assert!(!text.contains("\"x\":\"m1\""), "m1 should be skipped:\n{}", text);
        assert!(!text.contains("\"x\":\"m2\""), "m2 should be skipped");
        assert!(!text.contains("\"x\":\"m3\""), "m3 should be skipped");
        assert!(text.contains("\"x\":\"m4\""), "m4 should appear");
        assert!(text.contains("\"x\":\"m5\""), "m5 should appear");
        // ID は連動して進む
        assert!(text.contains("id: 4"));
        assert!(text.contains("id: 5"));
    }

    // ─────────────────────────────────────────────────────────────────
    // Tier 1 D: スロークライアント切断
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn slow_subscriber_terminated_by_lagged() {
        use futures_util::StreamExt;

        // 小さい capacity の broadcast を直接作って Lagged を再現
        let (tx, _) = broadcast::channel::<StoredEvent>(4);
        let state = Arc::new(RemoteState {
            history: Mutex::new(VecDeque::new()),
            last_partial: Mutex::new(HashMap::new()),
            broadcaster: tx,
            next_id: AtomicU64::new(1),
            history_size: 100,
            include_partials: false,
            partial_throttle: Duration::from_millis(100),
            auth_token: String::new(),
            basic_auth_user: String::new(),
            basic_auth_pass: String::new(),
            anchor: "2025-01-01T00:00:00Z".to_string(),
            channels: vec![],
            segments: SegmentStore::new(false),
            live: None,
            status: StatusStore::new(),
            index: Page::index(),
            access: AccessControl::disabled(),
            gate: None,
        });

        // build_stream を直接駆動して、Lagged で break することを検証
        let stream = build_stream(state.clone(), SubscribeMode::All);
        let mut pinned = Box::pin(stream);

        // hello を取得(まだ subscribe されていない)
        let hello = tokio::time::timeout(Duration::from_secs(1), pinned.next())
            .await
            .expect("hello timeout")
            .expect("hello none");
        let _ = hello;

        // 残りの stream を別タスクで消費(subscribe → recv ループへ進む)
        let task = tokio::spawn(async move {
            let mut count = 0;
            while let Some(_ev) = pinned.next().await {
                count += 1;
                if count > 100 {
                    break;
                }
            }
            count
        });

        // タスクが subscribe + recv.await にたどり着くまで待つ
        tokio::time::sleep(Duration::from_millis(50)).await;

        // capacity=4 の何倍も push → Lagged
        for i in 0..30 {
            state.push(&make_result(0, &format!("m{}", i), false));
        }

        // タスクは Lagged を受けて break するはず
        let count = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("stream did not terminate within 2 seconds")
            .unwrap();

        assert!(
            count < 30,
            "expected stream to terminate from Lagged, but received all {} events",
            count
        );
    }

    #[test]
    fn wire_event_omits_p_when_false() {
        let ev = WireEvent {
            c: 1,
            s: 2.5,
            x: "hi".to_string(),
            p: false,
            g: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("\"p\""));
        assert!(json.contains("\"c\":1"));
    }

    #[test]
    fn wire_event_includes_p_when_true() {
        let ev = WireEvent {
            c: 1,
            s: 2.5,
            x: "hi".to_string(),
            p: true,
            g: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"p\":true"));
    }

    #[test]
    fn event_ids_are_monotonic() {
        let s = make_state(false, 100);
        for i in 0..5 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        let hist = s.history.lock().unwrap();
        for i in 1..hist.len() {
            assert!(hist[i].id > hist[i - 1].id);
        }
    }

    #[test]
    fn subscribe_all_returns_full_history() {
        let s = make_state(false, 100);
        for i in 0..5 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        let (snap, _) = s.subscribe(SubscribeMode::All);
        assert_eq!(snap.len(), 5);
    }

    #[test]
    fn subscribe_since_filters_by_last_event_id() {
        let s = make_state(false, 100);
        for i in 0..5 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // ID=2 以降を要求 → id=3,4,5
        let (snap, _) = s.subscribe(SubscribeMode::Since(2));
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].id, 3);
        assert_eq!(snap[2].id, 5);

        // ID=999 以降 → 空
        let (snap, _) = s.subscribe(SubscribeMode::Since(999));
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn subscribe_since_with_old_id_falls_back_to_full_history() {
        let s = make_state(false, 3); // 小さい history
        for i in 0..10 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // last_event_id=1 だが history からは脱落済み(history は最後の3件のみ)。
        // フィルタ条件 e.id > 1 は全件マッチするので、現在の history 全部が返る。
        // graceful fallback: クライアントは欠損を含む可能性があるが重複はしない。
        let (snap, _) = s.subscribe(SubscribeMode::Since(1));
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn subscribe_head_returns_only_latest_n() {
        let s = make_state(false, 100);
        for i in 0..20 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // Head(5) → 最新の5件(id=16..20)のみ
        let (snap, _) = s.subscribe(SubscribeMode::Head(5));
        assert_eq!(snap.len(), 5);
        assert_eq!(snap[0].id, 16);
        assert_eq!(snap[4].id, 20);
        assert_eq!(snap[4].event.x, "m19");
    }

    #[test]
    fn subscribe_head_with_n_larger_than_history_returns_all() {
        let s = make_state(false, 100);
        for i in 0..3 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // Head(100) を要求しても history が3件しかなければ3件
        let (snap, _) = s.subscribe(SubscribeMode::Head(100));
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn history_before_returns_older_entries() {
        let s = make_state(false, 100);
        for i in 0..20 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // before=10, limit=5 → id=5..9 (5件)
        let res = s.history_before(10, 5);
        assert_eq!(res.len(), 5);
        assert_eq!(res[0].id, 5);
        assert_eq!(res[4].id, 9);
    }

    #[test]
    fn history_before_returns_empty_when_no_older() {
        let s = make_state(false, 100);
        for i in 0..3 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // 最古の id=1 より小さいものを要求 → 空
        let res = s.history_before(1, 50);
        assert_eq!(res.len(), 0);
    }

    #[test]
    fn history_before_caps_at_limit() {
        let s = make_state(false, 100);
        for i in 0..50 {
            s.push(&make_result(0, &format!("m{}", i), false));
        }
        // before=999 で全部取りに行くが limit=10 でキャップ → 最新側10件
        let res = s.history_before(999, 10);
        assert_eq!(res.len(), 10);
        assert_eq!(res[0].id, 41);
        assert_eq!(res[9].id, 50);
    }

    // ─────────────────────────────────────────────────────────────────
    // 録音区間
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn parse_range_variants() {
        assert_eq!(parse_range("bytes=0-1", 10), Some((0, 1)));
        assert_eq!(parse_range("bytes=0-", 10), Some((0, 9)));
        assert_eq!(parse_range("bytes=4-100", 10), Some((4, 9)));
        assert_eq!(parse_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(parse_range("bytes=-30", 10), Some((0, 9)));
        assert_eq!(parse_range("bytes=10-", 10), None);
        assert_eq!(parse_range("bytes=5-2", 10), None);
        assert_eq!(parse_range("bytes=-0", 10), None);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), None);
        assert_eq!(parse_range("items=0-1", 10), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }

    fn state_with_segment(dir: &std::path::Path) -> Arc<RemoteState> {
        let mut s = make_state(false, 10);
        Arc::get_mut(&mut s).unwrap().segments = SegmentStore::new(true);
        let path = dir.join("seg.aac");
        std::fs::write(&path, b"0123456789").unwrap();
        s.push_segment(&SegmentInfo {
            id: 7,
            channel: 2,
            start_unix_ms: 1000,
            duration_ms: 500,
            path,
        });
        s
    }

    #[test]
    fn transcript_carries_segment_id() {
        let s = make_state(false, 10);
        s.push_with_segment(&make_result(0, "a", false), Some(3));
        s.push(&make_result(0, "b", false));
        let (snap, _) = s.subscribe(SubscribeMode::All);
        assert_eq!(snap[0].event.g, Some(3));
        assert_eq!(snap[1].event.g, None);
        assert!(!serde_json::to_string(&snap[1].event).unwrap().contains("\"g\""));
    }

    #[test]
    fn segment_store_capped() {
        let s = make_state(false, 10);
        for i in 1..=(MAX_SEGMENTS as u64 + 5) {
            s.push_segment(&SegmentInfo {
                id: i,
                channel: 0,
                start_unix_ms: 0,
                duration_ms: 0,
                path: "x.wav".into(),
            });
        }
        let all = s.segments.after(0, None);
        assert_eq!(all.len(), MAX_SEGMENTS);
        assert_eq!(all[0].i, 6);
        assert_eq!(s.segments.after(MAX_SEGMENTS as u64, None).len(), 5);
    }

    #[tokio::test]
    async fn http_segments_lists_after() {
        let dir = tempfile::tempdir().unwrap();
        let addr = boot(state_with_segment(dir.path())).await;
        let body: serde_json::Value = reqwest::get(format!("http://{}/segments", addr))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            body["items"],
            serde_json::json!([{ "i": 7, "c": 2, "t": 1000, "d": 500 }])
        );
        let body: serde_json::Value = reqwest::get(format!("http://{}/segments?after=7", addr))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn http_audio_full_and_range() {
        let dir = tempfile::tempdir().unwrap();
        let addr = boot(state_with_segment(dir.path())).await;
        let client = reqwest::Client::new();

        let resp = client.get(format!("http://{}/audio/7", addr)).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "audio/aac");
        assert_eq!(resp.headers()["accept-ranges"], "bytes");
        assert!(resp.headers()["cache-control"].to_str().unwrap().starts_with("private"));
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"0123456789");

        let resp = client
            .get(format!("http://{}/audio/7", addr))
            .header("Range", "bytes=0-1")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers()["content-range"], "bytes 0-1/10");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"01");

        let resp = client
            .get(format!("http://{}/audio/7", addr))
            .header("Range", "bytes=20-")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 416);
    }

    #[tokio::test]
    async fn http_audio_unknown_id_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let addr = boot(state_with_segment(dir.path())).await;
        let resp = reqwest::get(format!("http://{}/audio/8", addr)).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn http_audio_requires_auth() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = state_with_segment(dir.path());
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        m.auth_token = "tok".to_string();
        let addr = boot(s).await;
        let client = reqwest::Client::new();

        let resp = client.get(format!("http://{}/audio/7?t=tok", addr)).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .get(format!("http://{}/audio/7", addr))
            .basic_auth("alice", Some("secret"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .get(format!("http://{}/audio/7?t=tok", addr))
            .basic_auth("alice", Some("secret"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn sse_delivers_seg_event() {
        use futures_util::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let s = state_with_segment(dir.path());
        let mut stream = Box::pin(build_stream(s.clone(), SubscribeMode::All));
        // hello を受けた時点で区間の購読は始まっている
        let _hello = stream.next().await.unwrap();
        s.push_segment(&SegmentInfo {
            id: 8,
            channel: 0,
            start_unix_ms: 0,
            duration_ms: 1,
            path: "x.wav".into(),
        });
        let ev = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = format!("{:?}", ev);
        assert!(text.contains("seg"), "{}", text);
        assert!(text.contains("\\\"i\\\":8"), "{}", text);
    }

    // ─────────────────────────────────────────────────────────────────
    // ライブ再生
    // ─────────────────────────────────────────────────────────────────

    fn state_with_live() -> (Arc<RemoteState>, Arc<LiveHub>) {
        use crate::aac::AacProfile;
        let hub = LiveHub::new(48000, 48000, AacProfile::Lc, vec![2, 3]);
        let mut s = make_state(false, 10);
        Arc::get_mut(&mut s).unwrap().live = Some(hub.clone());
        (s, hub)
    }

    #[tokio::test]
    async fn http_live_streams_aac() {
        use futures_util::StreamExt;
        let (s, hub) = state_with_live();
        let addr = boot(s).await;

        let resp = reqwest::get(format!("http://{}/live?c=2,3", addr)).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "audio/aac");
        assert_eq!(resp.headers()["cache-control"], "no-store");

        // 購読後に入力を流す(実際は入力側のタスクが呼ぶ)
        let feeder = tokio::spawn(async move {
            for _ in 0..200 {
                hub.process(&crate::live::LiveFrame {
                    channels: vec![(2, vec![1000; 480]), (3, vec![-500; 480])],
                });
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let mut body = resp.bytes_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first[0], 0xFF);
        assert_eq!(first[1] & 0xF0, 0xF0);
        feeder.abort();
    }

    #[tokio::test]
    async fn http_live_rejects_bad_channels() {
        let (s, _hub) = state_with_live();
        let addr = boot(s).await;
        for q in ["", "c=", "c=1", "c=2,x", "c=2,9"] {
            let resp = reqwest::get(format!("http://{}/live?{}", addr, q)).await.unwrap();
            assert_eq!(resp.status(), 400, "query {:?}", q);
        }
    }

    #[tokio::test]
    async fn http_live_disabled_is_404() {
        let addr = boot(make_state(false, 10)).await;
        let resp = reqwest::get(format!("http://{}/live?c=0", addr)).await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn http_live_requires_auth() {
        let (mut s, _hub) = state_with_live();
        let m = Arc::get_mut(&mut s).unwrap();
        m.basic_auth_user = "alice".to_string();
        m.basic_auth_pass = "secret".to_string();
        let addr = boot(s).await;
        let resp = reqwest::get(format!("http://{}/live?c=2", addr)).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    // ─────────────────────────────────────────────────────────────────
    // 転送量対策(brotli / ETag / 状態の差分配信 / 一覧の件数制限)
    // ─────────────────────────────────────────────────────────────────

    fn unbrotli(data: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        brotli::Decompressor::new(data, 4096).read_to_end(&mut out).unwrap();
        out
    }

    #[tokio::test]
    async fn http_index_is_brotli_with_etag() {
        let addr = boot(make_state(false, 10)).await;
        let client = reqwest::Client::new();

        let resp = client
            .get(format!("http://{}/", addr))
            .header("Accept-Encoding", "gzip, deflate, br")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-encoding"], "br");
        assert_eq!(resp.headers()["vary"], "Accept-Encoding");
        let etag = resp.headers()["etag"].to_str().unwrap().to_string();
        let body = resp.bytes().await.unwrap();
        assert!(body.len() < HTML_PAGE.len() / 3, "{} / {}", body.len(), HTML_PAGE.len());
        assert_eq!(unbrotli(&body), HTML_PAGE.as_bytes());

        // 変わっていなければ本文なしの 304
        let resp = client
            .get(format!("http://{}/", addr))
            .header("If-None-Match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 304);
        assert!(resp.bytes().await.unwrap().is_empty());

        // brotli 非対応なら非圧縮
        let resp = client.get(format!("http://{}/", addr)).send().await.unwrap();
        assert!(resp.headers().get("content-encoding").is_none());
        assert_eq!(resp.text().await.unwrap(), HTML_PAGE);
    }

    #[tokio::test]
    async fn http_history_compresses_only_large_bodies() {
        let s = make_state(false, 100);
        for i in 0..40 {
            s.push(&make_result(0, &format!("それなりに長い文字起こしの結果 {}", i), false));
        }
        let addr = boot(s).await;
        let client = reqwest::Client::new();
        let get = |q: &'static str| {
            client
                .get(format!("http://{}/history?{}", addr, q))
                .header("Accept-Encoding", "br")
                .send()
        };
        let big = get("before=1000&limit=40").await.unwrap();
        assert_eq!(big.headers()["content-encoding"], "br");
        let json: serde_json::Value = serde_json::from_slice(&unbrotli(&big.bytes().await.unwrap())).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 40);

        let small = get("before=3&limit=1").await.unwrap();
        assert!(small.headers().get("content-encoding").is_none());
    }

    #[test]
    fn segments_after_with_limit_returns_newest() {
        let s = make_state(false, 10);
        for i in 1..=10 {
            s.push_segment(&SegmentInfo {
                id: i,
                channel: 0,
                start_unix_ms: 0,
                duration_ms: 0,
                path: "x.aac".into(),
            });
        }
        let ids = |v: Vec<WireSegment>| v.iter().map(|w| w.i).collect::<Vec<_>>();
        assert_eq!(ids(s.segments.after(0, Some(3))), vec![8, 9, 10]);
        assert_eq!(ids(s.segments.after(8, Some(5))), vec![9, 10]);
        assert_eq!(ids(s.segments.after(0, None)).len(), 10);
    }

    fn st(channel: usize, level_db: f32, receiving: bool, transcribe: u8) -> ChannelStatus {
        ChannelStatus { channel, level_db, receiving, transcribe }
    }

    #[test]
    fn status_sent_only_on_meaningful_change() {
        let s = make_state(false, 10);
        let mut rx = s.status.broadcaster.subscribe();
        let mut recv = || rx.try_recv().ok().map(|a| a.to_string());

        s.push_status(&[st(0, -50.0, false, 0), st(1, -70.0, false, 0)]);
        // 初回は全チャンネル。音量は -60 で頭打ち
        assert_eq!(recv().as_deref(), Some("[[0,-50,0,0],[1,-60,0,0]]"));

        // 受信していない間の小さな揺れ(12 dB 未満)は送らない
        s.push_status(&[st(0, -42.0, false, 0), st(1, -60.0, false, 0)]);
        assert_eq!(recv(), None);

        // 受信開始は即送る。変化したチャンネルだけ
        s.push_status(&[st(0, -20.0, true, 1), st(1, -60.0, false, 0)]);
        assert_eq!(recv().as_deref(), Some("[[0,-20,1,1]]"));

        // 受信中は 6 dB 以上で送る
        s.push_status(&[st(0, -24.0, true, 1)]);
        assert_eq!(recv(), None);
        s.push_status(&[st(0, -27.0, true, 1)]);
        assert_eq!(recv().as_deref(), Some("[[0,-27,1,1]]"));

        // 接続直後用の現状は全チャンネル
        assert_eq!(s.status.snapshot().as_deref(), Some("[[0,-27,1,1],[1,-60,0,0]]"));
    }

    #[tokio::test]
    async fn sse_sends_status_snapshot_and_updates() {
        use futures_util::StreamExt;
        let s = make_state(false, 10);
        s.push_status(&[st(2, -30.0, true, 1)]);
        let mut stream = Box::pin(build_stream(s.clone(), SubscribeMode::All));
        let _hello = stream.next().await.unwrap();
        let snap = format!("{:?}", stream.next().await.unwrap().unwrap());
        assert!(snap.contains("st") && snap.contains("[[2,-30,1,1]]"), "{}", snap);

        s.push_status(&[st(2, -60.0, false, 0)]);
        let ev = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(format!("{:?}", ev).contains("[[2,-60,0,0]]"));
    }

    // ─────────────────────────────────────────────────────────────────
    // アクセス制限(国 / Turnstile)
    // ─────────────────────────────────────────────────────────────────

    /// 国制限 JP + Turnstile(siteverify は mockito)の状態
    async fn state_with_access(server: &mockito::ServerGuard) -> Arc<RemoteState> {
        let mut s = make_state(false, 10);
        let m = Arc::get_mut(&mut s).unwrap();
        let mut access = AccessControl::new(&["JP".into()], "site-key-x", "secret", Duration::from_secs(60));
        access.set_verify_url(&server.url());
        m.gate = Page::gate(&access);
        m.access = access;
        s
    }

    #[tokio::test]
    async fn http_country_restriction() {
        let server = mockito::Server::new_async().await;
        let addr = boot(state_with_access(&server).await).await;
        let client = reqwest::Client::new();
        let get = |country: Option<&'static str>, host: &'static str| {
            let mut r = client.get(format!("http://{}/", addr)).header("Host", host);
            if let Some(c) = country {
                r = r.header("CF-IPCountry", c);
            }
            r.send()
        };
        // 国外はトップから 403
        assert_eq!(get(Some("US"), "t.example").await.unwrap().status(), 403);
        // ヘッダ無しは公開ホスト名なら 403、ローカルなら通す
        assert_eq!(get(None, "t.example").await.unwrap().status(), 403);
        assert_eq!(get(None, "127.0.0.1:8765").await.unwrap().status(), 200);
        assert_eq!(get(Some("JP"), "t.example").await.unwrap().status(), 200);
        // /healthz は国に関係なく通す(外部監視用)
        let r = client
            .get(format!("http://{}/healthz", addr))
            .header("CF-IPCountry", "US")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn http_turnstile_gate_then_session() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({ "response": "ok-token" })))
            .with_body(r#"{"success":true}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({ "response": "bot" })))
            .with_body(r#"{"success":false,"error-codes":["invalid-input-response"]}"#)
            .create_async()
            .await;
        let addr = boot(state_with_access(&server).await).await;
        let client = reqwest::Client::new();
        let jp = |r: reqwest::RequestBuilder| r.header("CF-IPCountry", "JP");

        // セッション無し: トップは確認画面、データ系は 403
        let gate = jp(client.get(format!("http://{}/", addr))).send().await.unwrap();
        assert_eq!(gate.status(), 200);
        assert_eq!(gate.headers()["cache-control"], "no-store");
        let html = gate.text().await.unwrap();
        assert!(html.contains("site-key-x") && html.contains("turnstile"));
        for path in ["/stream", "/history?before=1", "/segments", "/audio/1", "/live?c=0", "/session"] {
            let r = jp(client.get(format!("http://{}{}", addr, path))).send().await.unwrap();
            assert_eq!(r.status(), 403, "{}", path);
        }

        // bot と判定されたらクッキーは出ない
        let r = jp(client.post(format!("http://{}/verify", addr)))
            .json(&serde_json::json!({ "token": "bot" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        assert!(r.headers().get("set-cookie").is_none());

        // 確認を通るとクッキー → 本画面とデータ系が使える
        let r = jp(client.post(format!("http://{}/verify", addr)))
            .json(&serde_json::json!({ "token": "ok-token" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204);
        let cookie = r.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
        assert!(cookie.starts_with("dcr_session="));

        let with = |path: &str| jp(client.get(format!("http://{}{}", addr, path))).header("Cookie", &cookie).send();
        assert_eq!(with("/").await.unwrap().text().await.unwrap(), HTML_PAGE);
        assert_eq!(with("/session").await.unwrap().status(), 204);
        assert_eq!(with("/segments").await.unwrap().status(), 200);
        // 国外からはクッキーがあっても 403
        let r = client
            .get(format!("http://{}/session", addr))
            .header("CF-IPCountry", "US")
            .header("Cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
}
