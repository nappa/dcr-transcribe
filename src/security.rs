//! 閲覧サーバの HTTP の守り(応答ヘッダ・CSP・Fetch Metadata・CSP 違反の報告)
//!
//! # 方針
//!
//! - **CSP**: 既定はすべて拒否(`default-src 'none'`)。ページのインラインの `<script>` / `<style>` は、
//!   起動時に中身の sha256 を計算してそれだけを許可する(`'unsafe-inline'` は使わない)。
//!   閲覧画面は外部のものを一切読まないので、`'self'` も画像・音声・通信に必要な分だけ。
//!   閲覧画面には Trusted Types も掛け、`innerHTML` などへの文字列の代入をブラウザに拒否させる
//! - **Fetch Metadata**: Sec-Fetch-* を送るブラウザを前提に、エンドポイントごとに「誰が・何として」読むかを決める
//!   (`/` はページ遷移、`/audio` は `<audio>`、QR は `<img>`、データ系は同じオリジンの fetch だけ)。
//!   別サイトから来た要求はトップレベルのページ遷移(ログインのリンクを開く)だけ通す(CSRF・XS-Leaks 対策)
//! - **ヘッダ**: クリックジャッキング・MIME の推測・Referer・別オリジンからの読み込み・
//!   ブラウザの機能(カメラ・位置情報など)を全部閉じる。HTTPS のときは HSTS
//! - **報告**: Reporting API の報告(CSP 違反・NEL のネットワークエラー・COOP / COEP 違反・非推奨・介入・クラッシュ)を
//!   `/reports` で、古い形式の CSP 違反を `/csp-report` で受け、種類ごとに要点を 1 行でログに出す
//!   (target `report`、1 分に `REPORT_LOGS_PER_MINUTE` 件まで)。NEL は HTTPS のときだけ
//! - **キャッシュ**: Cloudflare などの CDN には何も残させない(`CDN-Cache-Control` / `Cloudflare-CDN-Cache-Control: no-store`)

use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// HSTS の期間(2 年)。サブドメインまでは縛らない(同じドメインのほかのサービスに影響するため。
/// 必要なら Cloudflare の HSTS 設定で includeSubDomains / preload を付ける)
const HSTS: &str = "max-age=63072000";
/// 1 分あたりにログへ出す CSP 違反の上限(攻撃や拡張機能で大量に来てもログが埋まらないように)
const REPORT_LOGS_PER_MINUTE: u64 = 20;
/// CSP 違反の報告の大きさの上限
pub(crate) const REPORT_MAX_BYTES: usize = 16 * 1024;

/// 使う機能だけ自分のオリジンに許し、あとは全部禁止する
///
/// - autoplay: ライブ・録音の再生(ユーザーの操作から始めるが、連続再生で次の録音へ進むときに要る)
/// - clipboard-write: 共有の「コピー」
/// - web-share: 共有シート
const PERMISSIONS_POLICY: &str = "accelerometer=(), ambient-light-sensor=(), attribution-reporting=(), \
autoplay=(self), bluetooth=(), browsing-topics=(), camera=(), clipboard-read=(), clipboard-write=(self), \
compute-pressure=(), display-capture=(), encrypted-media=(), fullscreen=(), gamepad=(), geolocation=(), \
gyroscope=(), hid=(), identity-credentials-get=(), idle-detection=(), interest-cohort=(), keyboard-map=(), \
local-fonts=(), magnetometer=(), microphone=(), midi=(), otp-credentials=(), payment=(), picture-in-picture=(), \
publickey-credentials-create=(), publickey-credentials-get=(), screen-wake-lock=(), serial=(), \
speaker-selection=(), storage-access=(), sync-xhr=(), usb=(), web-share=(self), window-management=(), \
xr-spatial-tracking=()";

/// ページ以外(JSON・音声・SVG など)の CSP。万一ブラウザがページとして開いても何も動かない
const RESOURCE_CSP: &str = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; sandbox";

/// CSP 違反の報告先(Reporting API と、古い report-uri の両方)
const REPORTING: &str = "report-uri /csp-report; report-to csp";
/// NEL(ネットワークエラーの報告)を覚えておかせる期間(30 日)
const NEL_MAX_AGE: u32 = 30 * 24 * 3600;
/// 検索エンジン・保存サービスに載せない
const ROBOTS: &str = "noindex, nofollow, noarchive, nosnippet, noimageindex";

/// ページの種類(CSP と COEP が変わる)
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PageKind {
    /// 閲覧画面: 外部のものは何も読まない。Trusted Types・COEP も掛ける
    Viewer,
    /// Turnstile の確認画面: challenges.cloudflare.com の script と iframe だけ足す
    Gate,
    /// ログインキーを同じオリジンの /login へ送る画面
    Login,
    /// ログインの案内・ログイン直後の中継ページ(小さな HTML)
    Simple,
}

/// ページの CSP(インラインの script / style のハッシュ入り)
pub(crate) fn page_csp(html: &str, kind: PageKind) -> String {
    let (scripts, styles) = inline_hashes(html);
    let src = |hashes: &[String], extra: &str| {
        let mut v: Vec<String> = hashes.iter().map(|h| format!("'sha256-{}'", h)).collect();
        if !extra.is_empty() {
            v.push(extra.to_string());
        }
        if v.is_empty() {
            "'none'".to_string()
        } else {
            v.join(" ")
        }
    };
    let common = "default-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    match kind {
        PageKind::Viewer => format!(
            "{common}; script-src {}; style-src {}; img-src 'self'; media-src 'self' data:; connect-src 'self'; \
             require-trusted-types-for 'script'; trusted-types 'none'; {REPORTING}",
            src(&scripts, ""),
            src(&styles, ""),
        ),
        PageKind::Gate => format!(
            // Turnstile は自分の iframe を作り、要素の style 属性を触ることがあるので属性の style だけ許す
            "{common}; script-src {}; style-src {}; style-src-attr 'unsafe-inline'; \
             frame-src https://challenges.cloudflare.com; connect-src 'self'; {REPORTING}",
            src(&scripts, "https://challenges.cloudflare.com"),
            src(&styles, ""),
        ),
        PageKind::Login => format!(
            "{common}; script-src {}; style-src {}; connect-src 'self'; {REPORTING}",
            src(&scripts, ""),
            src(&styles, ""),
        ),
        PageKind::Simple => format!(
            "{common}; script-src {}; style-src {}; {REPORTING}",
            src(&scripts, ""),
            src(&styles, ""),
        ),
    }
}

/// ページのインラインの `<script>`(src なし)と `<style>` の中身の sha256(base64)
pub(crate) fn inline_hashes(html: &str) -> (Vec<String>, Vec<String>) {
    let hash = |s: &str| base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()));
    let blocks = |open: &str, close: &str, skip_src: bool| {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + open.len()..];
            let Some(tag_end) = after.find('>') else { break };
            let attrs = &after[..tag_end];
            // `<scriptX` のような別のタグは飛ばす
            let is_tag = attrs.is_empty() || attrs.starts_with(char::is_whitespace);
            let body = &after[tag_end + 1..];
            let Some(end) = body.find(close) else { break };
            if is_tag && !(skip_src && attrs.contains("src=")) {
                out.push(hash(&body[..end]));
            }
            rest = &body[end + close.len()..];
        }
        out
    };
    (blocks("<script", "</script>", true), blocks("<style", "</style>", false))
}

/// 全部の応答に付ける守りのヘッダ。ページの CSP は各ハンドラが付けたものを優先する
pub(crate) async fn headers_middleware(req: Request, next: Next) -> Response {
    let https = crate::access::is_https(req.headers());
    let origin = own_origin(req.headers()).filter(|o| o.starts_with("https://"));
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let set = |h: &mut HeaderMap, name: header::HeaderName, value: &'static str| {
        h.entry(name).or_insert(HeaderValue::from_static(value));
    };
    set(h, header::CONTENT_SECURITY_POLICY, RESOURCE_CSP);
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::X_FRAME_OPTIONS, "DENY");
    set(h, header::REFERRER_POLICY, "no-referrer");
    set(h, header::HeaderName::from_static("permissions-policy"), PERMISSIONS_POLICY);
    set(h, header::HeaderName::from_static("cross-origin-opener-policy"), "same-origin; report-to=\"default\"");
    set(h, header::HeaderName::from_static("cross-origin-resource-policy"), "same-origin");
    set(h, header::HeaderName::from_static("origin-agent-cluster"), "?1");
    set(h, header::HeaderName::from_static("x-permitted-cross-domain-policies"), "none");
    set(h, header::X_DNS_PREFETCH_CONTROL, "off");
    set(h, header::HeaderName::from_static("reporting-endpoints"), "default=\"/reports\", csp=\"/reports\"");
    set(h, header::HeaderName::from_static("x-robots-tag"), ROBOTS);
    // ブラウザのキャッシュは各応答の Cache-Control に任せ、CDN(Cloudflare)には何も残させない
    set(h, header::HeaderName::from_static("cdn-cache-control"), "no-store");
    set(h, header::HeaderName::from_static("cloudflare-cdn-cache-control"), "no-store");
    if https {
        set(h, header::STRICT_TRANSPORT_SECURITY, HSTS);
        // NEL: 閲覧者のブラウザが通信に失敗したら、つながったときにまとめて報告させる(「端末側かサーバ側か」の事後の調べ用)。
        // NEL の報告先は古い Report-To ヘッダで、絶対 URL の HTTPS が要る
        if let Some(origin) = origin {
            let report_to = format!(
                "{{\"group\":\"nel\",\"max_age\":{},\"endpoints\":[{{\"url\":\"{}/reports\"}}]}}",
                NEL_MAX_AGE, origin
            );
            if let Ok(v) = HeaderValue::from_str(&report_to) {
                h.entry(header::HeaderName::from_static("report-to")).or_insert(v);
            }
            let nel = format!(
                "{{\"report_to\":\"nel\",\"max_age\":{},\"success_fraction\":0.0,\"failure_fraction\":1.0}}",
                NEL_MAX_AGE
            );
            if let Ok(v) = HeaderValue::from_str(&nel) {
                h.entry(header::HeaderName::from_static("nel")).or_insert(v);
            }
        }
    }
    resp
}

/// 閲覧画面だけに付ける COEP(読むものは全部同じオリジンなので壊れない。Spectre 系の読み取りの対策)
pub(crate) fn viewer_isolation() -> [(header::HeaderName, &'static str); 1] {
    [(header::HeaderName::from_static("cross-origin-embedder-policy"), "require-corp; report-to=\"default\"")]
}

/// Fetch Metadata の判定
#[derive(Debug, PartialEq)]
pub(crate) enum FetchCheck {
    Ok,
    /// 規則に合わない(理由)
    Refuse(&'static str),
    /// Sec-Fetch-* が無い(対応していないブラウザ・curl など)
    Missing,
}

/// エンドポイントごとの Fetch Metadata の規則(Sec-Fetch-* を送るブラウザが前提)
///
/// | パス | 許すもの |
/// |---|---|
/// | `/` | ページ遷移(`navigate` + `document`)。別サイトからのトップレベルの遷移も(ログインのリンク) |
/// | `/audio/{id}` | 同じオリジンの `<audio>`(`audio` / `video`) |
/// | `/live` | 同じオリジンの `<audio>` と fetch(`audio` / `video` / `empty`) |
/// | `/share/qr.svg` | 同じオリジンの `<img>` |
/// | `/csp-report` `/reports` | ブラウザが送る報告(`report` / `empty`) |
/// | ほか(データ系・POST) | 同じオリジンの fetch / EventSource(`empty`、`navigate` 以外) |
pub(crate) fn check_fetch(method: &Method, path: &str, headers: &HeaderMap) -> FetchCheck {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let Some(site) = get("sec-fetch-site") else { return FetchCheck::Missing };
    let mode = get("sec-fetch-mode").unwrap_or("");
    let dest = get("sec-fetch-dest").unwrap_or("");
    let same_origin = site == "same-origin";
    let ok = |cond: bool, why: &'static str| if cond { FetchCheck::Ok } else { FetchCheck::Refuse(why) };
    match path {
        "/" => ok(
            (method == Method::GET || method == Method::HEAD) && mode == "navigate" && dest == "document",
            "ページ遷移ではない(埋め込み・fetch)",
        ),
        "/csp-report" | "/reports" => ok(
            matches!(site, "same-origin" | "none") && matches!(dest, "report" | "empty"),
            "報告ではない",
        ),
        "/live" => ok(same_origin && matches!(dest, "audio" | "video" | "empty"), "ライブ音声の読み方ではない"),
        "/share/qr.svg" => ok(same_origin && dest == "image", "画像としての読み込みではない"),
        p if p.starts_with("/audio/") => ok(same_origin && matches!(dest, "audio" | "video"), "音声としての読み込みではない"),
        _ => ok(same_origin && dest == "empty" && mode != "navigate", "同じオリジンの fetch ではない"),
    }
}

/// Sec-Fetch-* が無くても通すパス(ブラウザの裏の送信で付かないことがあるもの)。
/// 報告と、iOS などでメディアの読み込みが別経路になる音声。どれもセッション・大きさの制限は別にかかる
pub(crate) fn metadata_optional(path: &str) -> bool {
    matches!(path, "/csp-report" | "/reports" | "/live") || path.starts_with("/audio/")
}

/// POST の Origin が自分と同じか(Origin が無いのは古いブラウザ・curl なので Fetch Metadata に任せる)
pub(crate) fn same_origin_post(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let scheme = if crate::access::is_https(headers) { "https" } else { "http" };
    origin == format!("{}://{}", scheme, host)
}

/// 自分のオリジン(`https://host`)。QR に載せる URL を自分のものに限るのに使う
pub(crate) fn own_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok())?;
    let scheme = if crate::access::is_https(headers) { "https" } else { "http" };
    Some(format!("{}://{}", scheme, host))
}

static REPORT_MINUTE: AtomicU64 = AtomicU64::new(0);
static REPORT_COUNT: AtomicU64 = AtomicU64::new(0);

/// ブラウザからの報告(`application/csp-report` か `application/reports+json`)。要点だけ 1 行にしてログに出す
pub(crate) async fn serve_reports(headers: HeaderMap, body: Bytes) -> Response {
    let minute = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() / 60);
    if REPORT_MINUTE.swap(minute, Ordering::Relaxed) != minute {
        let dropped = REPORT_COUNT.swap(0, Ordering::Relaxed).saturating_sub(REPORT_LOGS_PER_MINUTE);
        if dropped > 0 {
            log::warn!(target: "report", "ブラウザからの報告をこの前の 1 分で {} 件省略しました", dropped);
        }
    }
    if REPORT_COUNT.fetch_add(1, Ordering::Relaxed) < REPORT_LOGS_PER_MINUTE {
        let ip = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or("-");
        for r in summarize_reports(&body) {
            log::warn!(target: "report", "ip={} {}", ip, r);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// 報告から要点を取り出す。長すぎる値は切る。種類ごとに見る項目を変える
fn summarize_reports(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return vec!["(読めない報告)".to_string()];
    };
    // report-uri: {"csp-report": {...}} / Reporting API: [{"type":"csp-violation","url":...,"body":{...}}, ...]
    let reports: Vec<(String, String, &serde_json::Value)> = match &v {
        serde_json::Value::Array(items) => items
            .iter()
            .take(10)
            .filter_map(|i| {
                let ty = i.get("type")?.as_str()?.to_string();
                let url = i.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                Some((ty, url, i.get("body")?))
            })
            .collect(),
        _ => v.get("csp-report").map(|b| ("csp-violation".to_string(), String::new(), b)).into_iter().collect(),
    };
    let field = |b: &serde_json::Value, keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| b.get(*k))
            .map(|x| match x {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect()
    };
    reports
        .into_iter()
        .map(|(ty, url, b)| {
            let detail = match ty.as_str() {
                "csp-violation" => format!(
                    "directive={} blocked={} document={} source={}:{} sample={:?}",
                    field(b, &["effectiveDirective", "effective-directive", "violated-directive"]),
                    field(b, &["blockedURL", "blocked-uri"]),
                    field(b, &["documentURL", "document-uri"]),
                    field(b, &["sourceFile", "source-file"]),
                    field(b, &["lineNumber", "line-number"]),
                    field(b, &["sample", "script-sample"]),
                ),
                // 例: type=tcp.timed_out phase=connection(閲覧者の回線からサーバへ届かなかった)
                "network-error" => format!(
                    "type={} phase={} status={} protocol={} elapsed={}ms server_ip={} method={}",
                    field(b, &["type"]),
                    field(b, &["phase"]),
                    field(b, &["status_code"]),
                    field(b, &["protocol"]),
                    field(b, &["elapsed_time"]),
                    field(b, &["server_ip"]),
                    field(b, &["method"]),
                ),
                "coep" | "coop" => format!("type={} blocked={} disposition={}", field(b, &["type"]), field(b, &["blockedURL"]), field(b, &["disposition"])),
                "deprecation" | "intervention" => format!(
                    "id={} message={:?} source={}:{}",
                    field(b, &["id"]),
                    field(b, &["message"]),
                    field(b, &["sourceFile"]),
                    field(b, &["lineNumber"]),
                ),
                "crash" => format!("reason={}", field(b, &["reason"])),
                "permissions-policy-violation" => format!("feature={} disposition={}", field(b, &["featureId"]), field(b, &["disposition"])),
                _ => String::new(),
            };
            let url: String = url.chars().take(200).collect();
            format!("{} {} url={}", ty, detail, url)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_inline_blocks_but_not_external_scripts() {
        let html = "<style>a{}</style><script>x()</script><script src=\"https://e/x.js\"></script><scripts>no</scripts>";
        let (scripts, styles) = inline_hashes(html);
        let h = |s: &str| base64::engine::general_purpose::STANDARD.encode(Sha256::digest(s.as_bytes()));
        assert_eq!(scripts, vec![h("x()")]);
        assert_eq!(styles, vec![h("a{}")]);
    }

    #[test]
    fn viewer_csp_is_strict() {
        let csp = page_csp("<style>a{}</style><script>x()</script>", PageKind::Viewer);
        assert!(csp.starts_with("default-src 'none'"), "{}", csp);
        assert!(!csp.contains("unsafe"), "unsafe-* を使わない: {}", csp);
        assert!(csp.contains("frame-ancestors 'none'") && csp.contains("base-uri 'none'") && csp.contains("form-action 'none'"));
        assert!(csp.contains("require-trusted-types-for 'script'"));
        assert!(!csp.contains("https://"), "外部のものは読まない: {}", csp);
        let gate = page_csp("<script>x()</script><script src=\"https://challenges.cloudflare.com/a.js\"></script>", PageKind::Gate);
        assert!(gate.contains("script-src 'sha256-") && gate.contains("https://challenges.cloudflare.com"), "{}", gate);
        assert!(gate.contains("style-src 'none'"), "{}", gate);
    }

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(*k, v.parse().unwrap());
        }
        m
    }

    fn meta(site: &str, mode: &str, dest: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("sec-fetch-site", site.parse().unwrap());
        m.insert("sec-fetch-mode", mode.parse().unwrap());
        m.insert("sec-fetch-dest", dest.parse().unwrap());
        m
    }

    #[test]
    fn fetch_rules_per_endpoint() {
        use FetchCheck::*;
        let get = Method::GET;
        // ページ: 直接・同じオリジン・別サイトからのトップレベルの遷移
        for site in ["none", "same-origin", "cross-site"] {
            assert_eq!(check_fetch(&get, "/", &meta(site, "navigate", "document")), Ok, "{}", site);
        }
        assert!(matches!(check_fetch(&get, "/", &meta("cross-site", "navigate", "iframe")), Refuse(_)), "iframe");
        assert!(matches!(check_fetch(&Method::POST, "/", &meta("cross-site", "navigate", "document")), Refuse(_)), "フォームの POST");
        // 音声は <audio> から
        assert_eq!(check_fetch(&get, "/audio/3", &meta("same-origin", "no-cors", "audio")), Ok);
        assert!(matches!(check_fetch(&get, "/audio/3", &meta("same-origin", "cors", "empty")), Refuse(_)), "fetch で音声を抜く");
        assert!(matches!(check_fetch(&get, "/audio/3", &meta("cross-site", "no-cors", "audio")), Refuse(_)), "よそのページの <audio>");
        assert_eq!(check_fetch(&get, "/live", &meta("same-origin", "cors", "empty")), Ok, "WebCodecs の fetch");
        assert_eq!(check_fetch(&get, "/live", &meta("same-origin", "no-cors", "audio")), Ok);
        // QR は <img> から
        assert_eq!(check_fetch(&get, "/share/qr.svg", &meta("same-origin", "no-cors", "image")), Ok);
        assert!(matches!(check_fetch(&get, "/share/qr.svg", &meta("same-origin", "navigate", "document")), Refuse(_)));
        // データ系は同じオリジンの fetch だけ
        assert_eq!(check_fetch(&get, "/history", &meta("same-origin", "cors", "empty")), Ok);
        assert!(matches!(check_fetch(&get, "/history", &meta("same-site", "cors", "empty")), Refuse(_)), "サブドメイン");
        assert!(matches!(check_fetch(&get, "/history", &meta("same-origin", "navigate", "document")), Refuse(_)), "直接開く");
        assert!(matches!(check_fetch(&get, "/stream", &meta("same-origin", "no-cors", "script")), Refuse(_)), "script として読む");
        // 報告
        assert_eq!(check_fetch(&Method::POST, "/reports", &meta("same-origin", "no-cors", "report")), Ok);
        // ヘッダが無い
        assert_eq!(check_fetch(&get, "/", &HeaderMap::new()), Missing);
        assert!(metadata_optional("/reports") && metadata_optional("/audio/1") && !metadata_optional("/history"));
    }

    #[test]
    fn origin_check_for_post() {
        let ok = h(&[("host", "r.example.com"), ("origin", "https://r.example.com"), ("x-forwarded-proto", "https")]);
        assert!(same_origin_post(&ok));
        let evil = h(&[("host", "r.example.com"), ("origin", "https://evil.example"), ("x-forwarded-proto", "https")]);
        assert!(!same_origin_post(&evil));
        let downgrade = h(&[("host", "r.example.com"), ("origin", "http://r.example.com"), ("x-forwarded-proto", "https")]);
        assert!(!same_origin_post(&downgrade));
        assert!(same_origin_post(&h(&[("host", "127.0.0.1:8765")])), "Origin の無い curl");
    }

    #[test]
    fn report_summaries_are_short() {
        let legacy = br#"{"csp-report":{"violated-directive":"script-src","blocked-uri":"https://evil/x.js","document-uri":"https://r/","script-sample":"alert(1)"}}"#;
        let s = summarize_reports(legacy);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("directive=script-src") && s[0].contains("blocked=https://evil/x.js"), "{}", s[0]);
        let modern = format!(r#"[{{"type":"csp-violation","body":{{"effectiveDirective":"img-src","blockedURL":"{}"}}}}]"#, "x".repeat(500));
        let s = summarize_reports(modern.as_bytes());
        assert!(s[0].len() < 400, "{}", s[0].len());
        assert_eq!(summarize_reports(b"garbage"), vec!["(読めない報告)"]);
        let nel = br#"[{"type":"network-error","url":"https://r/stream","body":{"type":"tcp.timed_out","phase":"connection","elapsed_time":30000,"server_ip":"104.16.0.1"}}]"#;
        let s = summarize_reports(nel);
        assert!(s[0].starts_with("network-error type=tcp.timed_out phase=connection"), "{}", s[0]);
        assert!(s[0].contains("url=https://r/stream"), "{}", s[0]);
    }
}
