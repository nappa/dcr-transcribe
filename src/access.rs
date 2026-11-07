//! リモート閲覧のアクセス制限
//!
//! - 国の制限: Cloudflare がトンネル経由のリクエストに付ける `CF-IPCountry` で判定する。
//!   ヘッダが無いのはローカル(`localhost` / `127.0.0.1`)から直接来たときだけ許可する。
//! - ログイン: `/#k=<ログインキー>` を開くと、画面の script がキーを同じオリジンの `POST /login` で渡してセッションクッキーをもらう。
//!   `#` 以降はサーバに送られないので、キーが Cloudflare・アクセスログ・リンクプレビューに渡らない
//!   (旧形式の `/?k=` も受け付ける)。
//!   キーは設定に直接書くか、ファイルに保存したもの(無ければ初回に生成)を使う
//! - bot の確認: Cloudflare Turnstile。確認画面を通過するとセッションが有効になる。
//!   ログインキーと併用する場合は、キーが正しい人だけ「確認待ち」になり、確認を通ると有効になる。
//! - データ系の API は有効なセッションが無いと 403。
//!   (Cloudflare のチャレンジ画面と違い、SSE や音声の読み込みでも期限切れで黙って止まらない)

use anyhow::{bail, Context, Result};
use axum::http::{header, HeaderMap};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// HTTP(LAN から直接見るとき)のセッションクッキー
pub const SESSION_COOKIE: &str = "dcr_session";
/// HTTPS(Cloudflare 経由)のセッションクッキー。`__Host-` 接頭辞のクッキーは Secure・Path=/・Domain なしでないと
/// ブラウザが受け付けないので、サブドメインや平文の HTTP から上書き・差し込みされない
pub const SECURE_SESSION_COOKIE: &str = "__Host-dcr_session";
const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// 保持するセッションの上限(ログイン・確認を通った人しか作れないが、念のため)
const MAX_SESSIONS: usize = 10_000;
/// 「確認待ち」セッションの有効時間(Turnstile の画面を開いてから通過するまで)
const PENDING_TTL: Duration = Duration::from_secs(600);
/// 生成するログインキーのバイト数(16 進で 2 倍の文字数になる)
const LOGIN_KEY_BYTES: usize = 16;

/// セッションの段階
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// クッキーが無い・無効・期限切れ
    None,
    /// ログインキーは通ったが Turnstile の確認がまだ
    Pending,
    /// 使える
    Full,
}

pub struct Turnstile {
    pub site_key: String,
    secret_key: String,
    verify_url: String,
    client: reqwest::Client,
}

pub struct AccessControl {
    /// 大文字の国コード。空なら制限しない
    allowed_countries: Vec<String>,
    /// ログインキー。空ならキー無しで使える
    login_key: String,
    turnstile: Option<Turnstile>,
    sessions: Mutex<HashMap<String, (Instant, SessionState)>>,
    session_ttl: Duration,
    /// Tor 経由(`CF-IPCountry: T1`)を拒否する
    block_tor: bool,
}

/// Cloudflare が Tor 経由のアクセスに付ける国コード
const TOR_COUNTRY: &str = "T1";

impl AccessControl {
    pub fn new(
        allowed_countries: &[String],
        login_key: &str,
        turnstile_site_key: &str,
        turnstile_secret_key: &str,
        session_ttl: Duration,
    ) -> Self {
        let turnstile = (!turnstile_site_key.is_empty() && !turnstile_secret_key.is_empty()).then(|| {
            Turnstile {
                site_key: turnstile_site_key.to_string(),
                secret_key: turnstile_secret_key.to_string(),
                verify_url: SITEVERIFY_URL.to_string(),
                client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .unwrap_or_default(),
            }
        });
        Self {
            allowed_countries: allowed_countries.iter().map(|c| c.trim().to_uppercase()).collect(),
            login_key: login_key.to_string(),
            turnstile,
            sessions: Mutex::new(HashMap::new()),
            session_ttl,
            block_tor: true,
        }
    }

    /// Tor 経由を拒否するか(既定は拒否)
    pub fn with_block_tor(mut self, block: bool) -> Self {
        self.block_tor = block;
        self
    }

    /// 制限なし(テスト用)
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::new(&[], "", "", "", Duration::from_secs(3600)).with_block_tor(false)
    }

    pub fn turnstile(&self) -> Option<&Turnstile> {
        self.turnstile.as_ref()
    }

    /// ログイン用のパス(`/#k=<キー>`、ログイン無しなら `/`)
    pub fn login_path(&self) -> String {
        if self.login_key.is_empty() {
            "/".to_string()
        } else {
            format!("/#k={}", self.login_key)
        }
    }

    /// 要求のセッションを消す(ログアウト)
    pub fn logout(&self, headers: &HeaderMap) {
        if let (Some(id), Ok(mut sessions)) = (session_id(headers), self.sessions.lock()) {
            sessions.remove(id);
        }
    }

    /// セッションクッキーを消すための Set-Cookie
    pub fn expired_cookie(https: bool) -> String {
        if https {
            format!("{}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict", SECURE_SESSION_COOKIE)
        } else {
            format!("{}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict", SESSION_COOKIE)
        }
    }

    pub fn login_required(&self) -> bool {
        !self.login_key.is_empty()
    }

    /// ログインも bot 確認も無い(誰でも使える)
    pub fn is_open(&self) -> bool {
        self.login_key.is_empty() && self.turnstile.is_none()
    }

    /// 起動ログ用の説明
    pub fn describe(&self) -> String {
        let countries = if self.allowed_countries.is_empty() {
            "制限なし".to_string()
        } else {
            self.allowed_countries.join(",")
        };
        format!(
            "ログイン={}, 国={}, Tor={}, bot確認={}",
            if self.login_required() { "キー" } else { "なし" },
            countries,
            if self.block_tor { "拒否" } else { "許可" },
            if self.turnstile.is_some() { "Turnstile" } else { "なし" }
        )
    }

    /// 国の制限を満たすか
    pub fn country_allowed(&self, headers: &HeaderMap) -> bool {
        let country = headers.get("cf-ipcountry").and_then(|v| v.to_str().ok());
        if self.block_tor && country.is_some_and(|c| c.trim().eq_ignore_ascii_case(TOR_COUNTRY)) {
            return false;
        }
        if self.allowed_countries.is_empty() {
            return true;
        }
        match country {
            Some(c) => self.allowed_countries.iter().any(|a| a.eq_ignore_ascii_case(c.trim())),
            // Cloudflare を通らない = ローカルから直接。トンネル経由なら Host は公開ホスト名になる
            None => is_local_host(headers),
        }
    }

    /// クッキーのセッションの段階
    pub fn session_state(&self, headers: &HeaderMap) -> SessionState {
        let Some(id) = session_id(headers) else {
            return SessionState::None;
        };
        let Ok(sessions) = self.sessions.lock() else {
            return SessionState::None;
        };
        match sessions.get(id) {
            Some((exp, state)) if *exp > Instant::now() => *state,
            _ => SessionState::None,
        }
    }

    /// データ系の API を使ってよいか(ログインも bot 確認も無ければ常に true)
    pub fn session_valid(&self, headers: &HeaderMap) -> bool {
        self.is_open() || self.session_state(headers) == SessionState::Full
    }

    /// ログインキーが正しいか(比較時間から推測されないよう一定時間で比べる)
    pub fn check_login_key(&self, key: &str) -> bool {
        !self.login_key.is_empty() && constant_time_eq(&self.login_key, key)
    }

    /// ログインキーが通ったときのセッション。Turnstile があれば「確認待ち」から始める
    pub fn login(&self) -> Result<String> {
        if self.turnstile.is_some() {
            self.new_session(SessionState::Pending, PENDING_TTL)
        } else {
            self.new_session(SessionState::Full, self.session_ttl)
        }
    }

    /// Turnstile のトークンを検証し、通ればセッション ID を返す。
    /// ログインキーを使う設定では「確認待ち」のセッションを持っている人だけ受け付ける
    pub async fn verify(&self, token: &str, remote_ip: Option<&str>, headers: &HeaderMap) -> Result<String> {
        let Some(ts) = &self.turnstile else {
            bail!("Turnstile が無効です");
        };
        if self.login_required() && self.session_state(headers) == SessionState::None {
            bail!("ログインしていません");
        }
        let mut body = serde_json::json!({ "secret": ts.secret_key, "response": token });
        if let Some(ip) = remote_ip {
            body["remoteip"] = serde_json::Value::from(ip);
        }
        let res: serde_json::Value = ts
            .client
            .post(&ts.verify_url)
            .json(&body)
            .send()
            .await
            .context("Turnstile の検証リクエストに失敗")?
            .json()
            .await
            .context("Turnstile の検証結果を読めません")?;
        if res["success"].as_bool() != Some(true) {
            bail!("Turnstile の検証に失敗: {}", res["error-codes"]);
        }
        let id = self.new_session(SessionState::Full, self.session_ttl)?;
        // 確認待ちのセッションは捨てて、新しい ID で有効なセッションを作る
        if let Some(old) = session_id(headers) {
            self.sessions.lock().map_err(|_| anyhow::anyhow!("セッションストアが利用できません"))?.remove(old);
        }
        Ok(id)
    }

    fn new_session(&self, state: SessionState, ttl: Duration) -> Result<String> {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("OS の乱数を取得できません: {}", e))?;
        let id: String = buf.iter().map(|b| format!("{:02x}", b)).collect();

        let now = Instant::now();
        let expires = now.checked_add(ttl).context("セッションの有効期間が大きすぎます")?;
        let mut sessions = self.sessions.lock().map_err(|_| anyhow::anyhow!("セッションストアが利用できません"))?;
        sessions.retain(|_, (exp, _)| *exp > now);
        if sessions.len() >= MAX_SESSIONS {
            // 期限の近いものから捨てる
            if let Some(oldest) = sessions.iter().min_by_key(|(_, (exp, _))| *exp).map(|(k, _)| k.clone()) {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(id.clone(), (expires, state));
        Ok(id)
    }

    /// セッションクッキーの Set-Cookie 値(有効期限はセッションの段階に合わせる)
    /// セッションクッキー。HTTPS なら `__Host-` + Secure、平文の HTTP(LAN)なら Secure なし(ブラウザが捨てるため)。
    /// どちらも HttpOnly・SameSite=Strict(別サイトからの要求には付かない)
    pub fn session_cookie(&self, id: &str, https: bool) -> String {
        let pending = self.sessions.lock().ok()
            .is_some_and(|sessions| matches!(sessions.get(id), Some((_, SessionState::Pending))));
        let ttl = if pending { PENDING_TTL } else { self.session_ttl };
        if https {
            format!("{}={}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Strict", SECURE_SESSION_COOKIE, id, ttl.as_secs())
        } else {
            format!("{}={}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict", SESSION_COOKIE, id, ttl.as_secs())
        }
    }

    #[cfg(test)]
    pub fn set_verify_url(&mut self, url: &str) {
        if let Some(ts) = &mut self.turnstile {
            ts.verify_url = url.to_string();
        }
    }
}

/// ログインキーを決める。設定に直接書いてあればそれを、無ければファイルから読み、
/// ファイルも無ければ生成して保存する(再起動してもログイン URL が変わらないように)
pub fn load_or_create_login_key(configured: &str, key_file: &str) -> Result<String> {
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    if key_file.is_empty() {
        return Ok(String::new());
    }
    let path = std::path::Path::new(key_file);
    if path.exists() {
        let key = std::fs::read_to_string(path)
            .with_context(|| format!("ログインキーを読めません: {}", key_file))?
            .trim()
            .to_string();
        if key.is_empty() {
            bail!("ログインキーのファイルが空です: {}", key_file);
        }
        return Ok(key);
    }
    let mut buf = [0u8; LOGIN_KEY_BYTES];
    getrandom::fill(&mut buf).context("OS の乱数を取得できません")?;
    let key: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("ディレクトリを作れません: {:?}", dir))?;
    }
    write_private(path, &format!("{}\n", key)).with_context(|| format!("ログインキーを保存できません: {}", key_file))?;
    log::info!("ログインキーを生成して保存しました: {}", key_file);
    Ok(key)
}

/// 秘密の文字列を比べる(比較時間から推測されないよう、長さが同じなら全バイト比べる)
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 本人だけ読み書きできるファイルとして書く
fn write_private(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(content.as_bytes())
}

/// HTTPS で来た要求か(cloudflared は元の要求の scheme を X-Forwarded-Proto で伝える)。
/// 偽っても、平文の HTTP では `__Host-` のクッキーをブラウザが受け付けないので害はない
pub fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').next().is_some_and(|p| p.trim().eq_ignore_ascii_case("https")))
}

/// 要求のセッション ID。HTTPS では `__Host-` のクッキーだけを見る(接頭辞なしの同名クッキーで差し替えられないように)
fn session_id(headers: &HeaderMap) -> Option<&str> {
    cookie(headers, if is_https(headers) { SECURE_SESSION_COOKIE } else { SESSION_COOKIE })
}

fn is_local_host(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(name, "localhost" | "127.0.0.1" | "[::1]")
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    fn jp_only() -> AccessControl {
        AccessControl::new(&["jp".to_string()], "", "", "", Duration::from_secs(60))
    }

    #[test]
    fn country_from_cloudflare_header() {
        let a = jp_only();
        assert!(a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "JP")])));
        assert!(!a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "US")])));
        // Tor や不明は通さない
        assert!(!a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "T1")])));
        assert!(!a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "XX")])));
    }

    #[test]
    fn missing_country_allowed_only_for_local_host() {
        let a = jp_only();
        assert!(a.country_allowed(&headers(&[("host", "127.0.0.1:8765")])));
        assert!(a.country_allowed(&headers(&[("host", "localhost")])));
        assert!(!a.country_allowed(&headers(&[("host", "transcribe.example.com")])));
        assert!(!a.country_allowed(&HeaderMap::new()));
    }

    #[test]
    fn no_country_restriction_when_empty() {
        let a = AccessControl::disabled();
        assert!(a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "US")])));
    }

    #[test]
    fn tor_blocked_even_without_country_list() {
        let a = AccessControl::new(&[], "", "", "", Duration::from_secs(60));
        assert!(!a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "T1")])));
        assert!(a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "US")])));
        let a = a.with_block_tor(false);
        assert!(a.country_allowed(&headers(&[("host", "x.example"), ("cf-ipcountry", "T1")])));
    }

    #[test]
    fn session_required_only_with_turnstile() {
        assert!(AccessControl::disabled().session_valid(&HeaderMap::new()));

        let a = AccessControl::new(&[], "", "site", "secret", Duration::from_secs(60));
        assert!(!a.session_valid(&HeaderMap::new()));
        let id = a.new_session(SessionState::Full, Duration::from_secs(60)).unwrap();
        assert_eq!(id.len(), 64);
        assert!(a.session_valid(&headers(&[("cookie", &format!("a=b; dcr_session={}", id))])));
        assert!(!a.session_valid(&headers(&[("cookie", "dcr_session=forged")])));
        assert_ne!(id, a.new_session(SessionState::Full, Duration::from_secs(60)).unwrap());
    }

    #[test]
    fn https_reads_only_host_prefixed_cookie() {
        let a = AccessControl::new(&[], "", "site", "secret", Duration::from_secs(60));
        let id = a.new_session(SessionState::Full, Duration::from_secs(60)).unwrap();
        let https = |c: String| headers(&[("x-forwarded-proto", "https"), ("cookie", &c)]);
        assert!(a.session_valid(&https(format!("__Host-dcr_session={}", id))));
        // HTTPS では接頭辞なしのクッキー(サブドメインや平文の HTTP から差し込めるもの)を信用しない
        assert!(!a.session_valid(&https(format!("dcr_session={}", id))));
        // 平文の HTTP(LAN)では接頭辞なし
        assert!(a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", id))])));
        assert!(!a.session_valid(&headers(&[("cookie", &format!("__Host-dcr_session={}", id))])));
    }

    #[test]
    fn session_expires() {
        let a = AccessControl::new(&[], "", "site", "secret", Duration::from_millis(0));
        let id = a.new_session(SessionState::Full, Duration::from_millis(0)).unwrap();
        assert!(!a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", id))])));
    }

    #[test]
    fn cookie_attributes() {
        let a = AccessControl::new(&[], "", "site", "secret", Duration::from_secs(3600));
        let c = a.session_cookie("abc", false);
        assert!(c.starts_with("dcr_session=abc;"));
        assert!(c.contains("SameSite=Strict") && c.contains("HttpOnly") && !c.contains("Secure"), "{}", c);
        let c = a.session_cookie("abc", true);
        assert!(c.starts_with("__Host-dcr_session=abc; Path=/;"), "{}", c);
        assert!(c.contains("Secure") && c.contains("SameSite=Strict") && !c.contains("Domain"), "{}", c);
        for attr in ["HttpOnly", "Secure", "SameSite=Strict", "Max-Age=3600", "Path=/"] {
            assert!(c.contains(attr), "{}", c);
        }
    }

    #[tokio::test]
    async fn verify_calls_siteverify() {
        let mut server = mockito::Server::new_async().await;
        let ok = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "secret": "secret", "response": "good", "remoteip": "203.0.113.1"
            })))
            .with_body(r#"{"success":true}"#)
            .create_async()
            .await;
        let ng = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({ "response": "bad" })))
            .with_body(r#"{"success":false,"error-codes":["invalid-input-response"]}"#)
            .create_async()
            .await;
        let mut a = AccessControl::new(&[], "", "site", "secret", Duration::from_secs(60));
        a.set_verify_url(&server.url());

        let none = HeaderMap::new();
        let id = a.verify("good", Some("203.0.113.1"), &none).await.unwrap();
        assert!(a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", id))])));
        let err = a.verify("bad", None, &none).await.unwrap_err().to_string();
        assert!(err.contains("invalid-input-response"), "{}", err);
        ok.assert_async().await;
        ng.assert_async().await;
    }

    #[test]
    fn login_key_compare() {
        let a = AccessControl::new(&[], "abc123", "", "", Duration::from_secs(60));
        assert!(a.check_login_key("abc123"));
        assert!(!a.check_login_key("abc124"));
        assert!(!a.check_login_key("abc12"));
        assert!(!a.check_login_key(""));
        // キー未設定ならどんな値も通さない(空文字も)
        assert!(!AccessControl::disabled().check_login_key(""));
    }

    #[test]
    fn login_without_turnstile_is_full() {
        let a = AccessControl::new(&[], "k", "", "", Duration::from_secs(60));
        assert!(!a.is_open());
        assert!(!a.session_valid(&HeaderMap::new()));
        let id = a.login().unwrap();
        let h = headers(&[("cookie", &format!("dcr_session={}", id))]);
        assert_eq!(a.session_state(&h), SessionState::Full);
        assert!(a.session_valid(&h));
    }

    #[tokio::test]
    async fn login_with_turnstile_needs_verification() {
        let mut server = mockito::Server::new_async().await;
        server.mock("POST", "/").with_body(r#"{"success":true}"#).create_async().await;
        let mut a = AccessControl::new(&[], "k", "site", "secret", Duration::from_secs(60));
        a.set_verify_url(&server.url());

        // ログイン前は確認も受け付けない
        assert!(a.verify("t", None, &HeaderMap::new()).await.is_err());

        let pending = a.login().unwrap();
        let hp = headers(&[("cookie", &format!("dcr_session={}", pending))]);
        assert_eq!(a.session_state(&hp), SessionState::Pending);
        assert!(!a.session_valid(&hp));
        assert!(a.session_cookie(&pending, false).contains("Max-Age=600"));

        let full = a.verify("t", None, &hp).await.unwrap();
        assert_ne!(full, pending);
        assert_eq!(a.session_state(&hp), SessionState::None, "確認待ちの ID は捨てる");
        assert!(a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", full))])));
    }

    #[test]
    fn login_key_file_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/login_key");
        let f = file.to_str().unwrap();
        let k1 = load_or_create_login_key("", f).unwrap();
        assert_eq!(k1.len(), LOGIN_KEY_BYTES * 2);
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // 再起動しても同じキー
        assert_eq!(load_or_create_login_key("", f).unwrap(), k1);
        // 設定に直接書いたものが優先
        assert_eq!(load_or_create_login_key("direct", f).unwrap(), "direct");
        // ファイル指定も無ければログイン無し
        assert_eq!(load_or_create_login_key("", "").unwrap(), "");
    }
    #[test]
    fn session_expiry_overflow_returns_error_without_inserting() {
        let a = AccessControl::disabled();
        assert!(a.new_session(SessionState::Full, Duration::MAX).is_err());
        assert!(a.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn poisoned_sessions_fail_closed_without_panicking() {
        let a = AccessControl::new(&[], "key", "", "", Duration::from_secs(60));
        let id = a.login().unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = a.sessions.lock().unwrap();
            panic!("injected");
        }));
        assert!(!a.session_valid(&headers(&[("cookie", &format!("dcr_session={id}"))])));
        assert!(a.login().is_err());
        assert!(a.session_cookie(&id, true).contains("HttpOnly"));
    }

}
