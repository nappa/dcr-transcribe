//! リモート閲覧のアクセス制限
//!
//! - 国の制限: Cloudflare がトンネル経由のリクエストに付ける `CF-IPCountry` で判定する。
//!   ヘッダが無いのはローカル(`localhost` / `127.0.0.1`)から直接来たときだけ許可する。
//! - bot の確認: Cloudflare Turnstile。閲覧画面を開くと確認画面を出し、通過したら
//!   セッションクッキーを発行する。データ系の API はこのクッキーが無いと 403 にする。
//!   (Cloudflare のチャレンジ画面と違い、SSE や音声の読み込みでも期限切れで黙って止まらない)

use anyhow::{bail, Context, Result};
use axum::http::{header, HeaderMap};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const SESSION_COOKIE: &str = "dcr_session";
const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
/// 保持するセッションの上限(確認を通った人しか作れないが、念のため)
const MAX_SESSIONS: usize = 10_000;

pub struct Turnstile {
    pub site_key: String,
    secret_key: String,
    verify_url: String,
    client: reqwest::Client,
}

pub struct AccessControl {
    /// 大文字の国コード。空なら制限しない
    allowed_countries: Vec<String>,
    turnstile: Option<Turnstile>,
    sessions: Mutex<HashMap<String, Instant>>,
    session_ttl: Duration,
}

impl AccessControl {
    pub fn new(
        allowed_countries: &[String],
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
            turnstile,
            sessions: Mutex::new(HashMap::new()),
            session_ttl,
        }
    }

    /// 制限なし(テスト用)
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::new(&[], "", "", Duration::from_secs(3600))
    }

    pub fn turnstile(&self) -> Option<&Turnstile> {
        self.turnstile.as_ref()
    }

    /// 起動ログ用の説明
    pub fn describe(&self) -> String {
        let countries = if self.allowed_countries.is_empty() {
            "制限なし".to_string()
        } else {
            self.allowed_countries.join(",")
        };
        format!(
            "国={}, bot確認={}",
            countries,
            if self.turnstile.is_some() { "Turnstile" } else { "なし" }
        )
    }

    /// 国の制限を満たすか
    pub fn country_allowed(&self, headers: &HeaderMap) -> bool {
        if self.allowed_countries.is_empty() {
            return true;
        }
        match headers.get("cf-ipcountry").and_then(|v| v.to_str().ok()) {
            Some(c) => self.allowed_countries.iter().any(|a| a.eq_ignore_ascii_case(c.trim())),
            // Cloudflare を通らない = ローカルから直接。トンネル経由なら Host は公開ホスト名になる
            None => is_local_host(headers),
        }
    }

    /// bot 確認済みのセッションを持っているか(Turnstile が無効なら常に true)
    pub fn session_valid(&self, headers: &HeaderMap) -> bool {
        if self.turnstile.is_none() {
            return true;
        }
        let Some(id) = cookie(headers, SESSION_COOKIE) else {
            return false;
        };
        let sessions = self.sessions.lock().unwrap();
        sessions.get(id).is_some_and(|exp| *exp > Instant::now())
    }

    /// Turnstile のトークンを検証し、通ればセッション ID を返す
    pub async fn verify(&self, token: &str, remote_ip: Option<&str>) -> Result<String> {
        let Some(ts) = &self.turnstile else {
            bail!("Turnstile が無効です");
        };
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
        Ok(self.new_session())
    }

    fn new_session(&self) -> String {
        let mut buf = [0u8; 32];
        // OS の乱数が取れない環境では起動自体が成り立たないので panic でよい
        getrandom::fill(&mut buf).expect("OS の乱数を取得できません");
        let id: String = buf.iter().map(|b| format!("{:02x}", b)).collect();

        let now = Instant::now();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, exp| *exp > now);
        if sessions.len() >= MAX_SESSIONS {
            // 期限の近いものから捨てる
            if let Some(oldest) = sessions.iter().min_by_key(|(_, exp)| **exp).map(|(k, _)| k.clone()) {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(id.clone(), now + self.session_ttl);
        id
    }

    /// セッションクッキーの Set-Cookie 値
    pub fn session_cookie(&self, id: &str) -> String {
        format!(
            "{}={}; Path=/; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
            SESSION_COOKIE,
            id,
            self.session_ttl.as_secs()
        )
    }

    #[cfg(test)]
    pub fn set_verify_url(&mut self, url: &str) {
        if let Some(ts) = &mut self.turnstile {
            ts.verify_url = url.to_string();
        }
    }
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
        AccessControl::new(&["jp".to_string()], "", "", Duration::from_secs(60))
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
    fn session_required_only_with_turnstile() {
        assert!(AccessControl::disabled().session_valid(&HeaderMap::new()));

        let a = AccessControl::new(&[], "site", "secret", Duration::from_secs(60));
        assert!(!a.session_valid(&HeaderMap::new()));
        let id = a.new_session();
        assert_eq!(id.len(), 64);
        assert!(a.session_valid(&headers(&[("cookie", &format!("a=b; dcr_session={}", id))])));
        assert!(!a.session_valid(&headers(&[("cookie", "dcr_session=forged")])));
        assert_ne!(id, a.new_session());
    }

    #[test]
    fn session_expires() {
        let a = AccessControl::new(&[], "site", "secret", Duration::from_millis(0));
        let id = a.new_session();
        assert!(!a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", id))])));
    }

    #[test]
    fn cookie_attributes() {
        let a = AccessControl::new(&[], "site", "secret", Duration::from_secs(3600));
        let c = a.session_cookie("abc");
        assert!(c.starts_with("dcr_session=abc;"));
        for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Max-Age=3600", "Path=/"] {
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
        let mut a = AccessControl::new(&[], "site", "secret", Duration::from_secs(60));
        a.set_verify_url(&server.url());

        let id = a.verify("good", Some("203.0.113.1")).await.unwrap();
        assert!(a.session_valid(&headers(&[("cookie", &format!("dcr_session={}", id))])));
        let err = a.verify("bad", None).await.unwrap_err().to_string();
        assert!(err.contains("invalid-input-response"), "{}", err);
        ok.assert_async().await;
        ng.assert_async().await;
    }
}
