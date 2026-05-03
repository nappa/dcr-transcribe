//! リモート閲覧用の HTTP/SSE サーバ
//!
//! `axum` で軽量サーバを localhost に立て、`ngrok http <port>` で公開する想定。
//!
//! # エンドポイント
//!
//! - `GET /` - HTML ページ(モバイル対応 SSE クライアント)
//! - `GET /stream` - Server-Sent Events
//!     - 接続時に `event: hello` で anchor 時刻とチャンネル一覧を送信
//!     - 続いて `event: msg` で履歴(直近 N 件、または `Last-Event-ID` 以降)を流す
//!     - 以降、新規イベントを `event: msg` で順次配信
//! - `GET /healthz` - "ok" 返却
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

use crate::config::{ChannelConfig, RemoteConfig};
use crate::types::TranscriptResult;
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
/// broadcast バッファ容量。スロークライアントが本数ぶん遅れた時点で Lagged として切断される。
/// 小さいほどスロークライアントを早く切るが、瞬間的なバースト時の誤切断も増える。
const BROADCAST_CAPACITY: usize = 256;

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
}

impl RemoteState {
    /// 文字起こし結果を投入(同期、ノンブロッキング)
    pub fn push(&self, result: &TranscriptResult) {
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
    });

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/stream", get(serve_stream))
        .route("/history", get(serve_history))
        .route("/healthz", get(serve_health))
        .with_state(state.clone());

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
        "Remote サーバ起動: http://{} (auth={}, partials={})",
        actual_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| config.listen.clone()),
        auth_desc,
        config.include_partials
    );

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            log::error!("Remote サーバが終了: {}", e);
        }
    });

    Ok(state)
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
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // 静的だがアプリと合わせて配信されるので軽くキャッシュする
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        HTML_PAGE,
    )
        .into_response()
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
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn build_stream(
    state: Arc<RemoteState>,
    mode: SubscribeMode,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        // 1. hello イベント(anchor + チャンネル名)
        let hello = Hello {
            anchor: state.anchor.clone(),
            channels: state.channels.clone(),
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

        // 4. ライブ配信
        loop {
            match rx.recv().await {
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
        let app = Router::new()
            .route("/", get(serve_index))
            .route("/stream", get(serve_stream))
            .route("/history", get(serve_history))
            .route("/healthz", get(serve_health))
            .with_state(state);
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
}
