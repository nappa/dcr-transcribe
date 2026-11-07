//! ブラウザの版の確認
//!
//! 閲覧画面を開いたとき(`/` のページ遷移)に、UA Client Hints(`Sec-CH-UA`、Chromium 系)か User-Agent から
//! エンジンと版を読み、下限より古ければ「対応していません」のページを返す。
//!
//! - User-Agent は偽れるので守りの境界ではない。修正の入っていない古いブラウザで見させないための衛生面の対策
//! - iPhone / iPad のブラウザはすべて WebKit なので Safari(WebKit)の版で見る。iOS 26 以降の Safari は
//!   User-Agent の OS の版を 18_6 に固定しているので、`Version/` があればそちらを使う
//! - 読めないブラウザは止めない(画面側の機能の確認に任せる)

use crate::config::BrowserPolicy;
use axum::http::HeaderMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Engine {
    /// Chrome・Edge・Opera・Samsung Internet・Android の WebView など
    Chromium,
    /// Safari と、iPhone / iPad のすべてのブラウザ
    WebKit,
    Firefox,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Chromium => "Chrome / Edge などの Chromium 系",
            Engine::WebKit => "Safari(iPhone / iPad のブラウザを含む)",
            Engine::Firefox => "Firefox",
        }
    }
}

/// 読み取ったエンジンと版(major, minor)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Detected {
    pub engine: Engine,
    pub version: (u32, u32),
}

/// エンジンと版を読む。読めなければ None
pub fn detect(headers: &HeaderMap) -> Option<Detected> {
    // Chromium 系は Sec-CH-UA を送る("Chromium";v="124" のブランドを見る。ほかのブランドは名前を偽ることがある)
    if let Some(ch) = headers.get("sec-ch-ua").and_then(|v| v.to_str().ok()) {
        if let Some(v) = ch_brand_version(ch, "Chromium") {
            return Some(Detected { engine: Engine::Chromium, version: (v, 0) });
        }
    }
    let ua = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok())?;
    detect_ua(ua)
}

fn ch_brand_version(ch: &str, brand: &str) -> Option<u32> {
    ch.split(',').find_map(|item| {
        let (name, rest) = item.trim().split_once(';')?;
        if name.trim().trim_matches('"') != brand {
            return None;
        }
        rest.trim().strip_prefix("v=")?.trim_matches('"').split('.').next()?.parse().ok()
    })
}

fn detect_ua(ua: &str) -> Option<Detected> {
    let num = |s: &str| -> Option<(u32, u32)> {
        let mut it = s.split(['.', '_']).map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>());
        let major = it.next()?.parse().ok()?;
        let minor = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        Some((major, minor))
    };
    let after = |token: &str| ua.find(token).map(|i| &ua[i + token.len()..]);
    let ios = ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod");
    if ios {
        // iPhone / iPad: どのブラウザも WebKit。Safari の Version/ を優先し、無ければ OS の版
        let version = after("Version/").and_then(num).or_else(|| after(" OS ").and_then(num))?;
        return Some(Detected { engine: Engine::WebKit, version });
    }
    if let Some(v) = after("Firefox/").and_then(num) {
        return Some(Detected { engine: Engine::Firefox, version: v });
    }
    // Chrome/ は Edge・Opera・Android の WebView にも入っている
    if let Some(v) = after("Chrome/").or_else(|| after("Chromium/")).and_then(num) {
        return Some(Detected { engine: Engine::Chromium, version: v });
    }
    if ua.contains("Safari/") && ua.contains("AppleWebKit/") {
        if let Some(v) = after("Version/").and_then(num) {
            return Some(Detected { engine: Engine::WebKit, version: v });
        }
    }
    None
}

/// 下限より古ければ、そのエンジンと必要な版を返す
pub fn too_old(headers: &HeaderMap, policy: &BrowserPolicy) -> Option<(Detected, String)> {
    let d = detect(headers)?;
    let (need, label) = match d.engine {
        Engine::Chromium => ((policy.min_chromium, 0), policy.min_chromium.to_string()),
        Engine::Firefox => ((policy.min_firefox, 0), policy.min_firefox.to_string()),
        Engine::WebKit => {
            let mut it = policy.min_safari.split('.');
            let major = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
            let minor = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
            ((major, minor), policy.min_safari.clone())
        }
    };
    (d.version < need).then_some((d, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, v.parse().unwrap());
        }
        m
    }

    fn ua(s: &str) -> Option<Detected> {
        detect(&h(&[("user-agent", s)]))
    }

    #[test]
    fn detects_major_browsers() {
        let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
        assert_eq!(ua(chrome), Some(Detected { engine: Engine::Chromium, version: (141, 0) }));
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
        assert_eq!(ua(edge).unwrap().engine, Engine::Chromium);
        let safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
        assert_eq!(ua(safari), Some(Detected { engine: Engine::WebKit, version: (26, 0) }));
        let firefox = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:143.0) Gecko/20100101 Firefox/143.0";
        assert_eq!(ua(firefox), Some(Detected { engine: Engine::Firefox, version: (143, 0) }));
        // iPhone の Chrome(中身は WebKit)。OS の版で見る
        let crios = "Mozilla/5.0 (iPhone; CPU iPhone OS 16_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0.6099.119 Mobile/15E148 Safari/604.1";
        assert_eq!(ua(crios), Some(Detected { engine: Engine::WebKit, version: (16, 3) }));
        // iOS 26 の Safari は OS の版を 18_6 に固定している。Version/ を見る
        let ios26 = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
        assert_eq!(ua(ios26).unwrap().version, (26, 0));
        // Android の LINE などのアプリ内ブラウザ(WebView)
        let line = "Mozilla/5.0 (Linux; Android 14; Pixel 8 Build/UQ1A; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/139.0.0.0 Mobile Safari/537.36 Line/15.0.0";
        assert_eq!(ua(line), Some(Detected { engine: Engine::Chromium, version: (139, 0) }));
        assert_eq!(ua("curl/8.7.1"), None);
    }

    #[test]
    fn client_hints_take_precedence() {
        let d = detect(&h(&[
            ("sec-ch-ua", "\"Not)A;Brand\";v=\"99\", \"Google Chrome\";v=\"127\", \"Chromium\";v=\"127\""),
            ("user-agent", "Mozilla/5.0 Chrome/999.0.0.0"),
        ]));
        assert_eq!(d, Some(Detected { engine: Engine::Chromium, version: (127, 0) }));
    }

    #[test]
    fn minimum_versions() {
        let policy = BrowserPolicy::default();
        let old_ios = h(&[("user-agent", "Mozilla/5.0 (iPhone; CPU iPhone OS 16_3 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.3 Mobile/15E148 Safari/604.1")]);
        assert_eq!(too_old(&old_ios, &policy).map(|(_, need)| need).as_deref(), Some("16.4"));
        let ok_ios = h(&[("user-agent", "Mozilla/5.0 (iPhone; CPU iPhone OS 16_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.4 Mobile/15E148 Safari/604.1")]);
        assert!(too_old(&ok_ios, &policy).is_none());
        let old_chrome = h(&[("sec-ch-ua", "\"Chromium\";v=\"100\"")]);
        assert!(too_old(&old_chrome, &policy).is_some());
        assert!(too_old(&h(&[("user-agent", "curl/8.7.1")]), &policy).is_none(), "読めないものは止めない");
    }
}
