//! 閲覧画面のログイン URL と QR コード(TUI と閲覧画面の「共有」で表示する)

use crate::config::{RemoteConfig, TunnelConfig};
use anyhow::{Context, Result};
use qrcode::{Color, QrCode};

/// QR コードの周囲の余白(モジュール数)。規格は 4 だが、端末に収めるため 2 にする(スマホのカメラなら読める)
const QUIET_ZONE: usize = 2;

/// TUI に表示するログイン URL
#[derive(Debug, Clone, PartialEq)]
pub struct ShareInfo {
    /// このパソコンから開く URL(`http://127.0.0.1:8765/?k=...`)
    pub local: String,
    /// 公開 URL(トンネル経由)。分からなければ None
    pub public: Option<String>,
}

impl ShareInfo {
    /// `login_path` は `/?k=<キー>`(ログイン無しなら `/`)
    pub fn new(remote: &RemoteConfig, tunnel: &TunnelConfig, login_path: &str) -> Self {
        Self {
            local: format!("http://{}{}", remote.listen, login_path),
            public: public_base(remote, tunnel).map(|base| format!("{}{}", base, login_path)),
        }
    }
}

/// 公開 URL の先頭部分(`https://host`)。設定の `public_url` が優先、無ければトンネルの設定ファイルの hostname
fn public_base(remote: &RemoteConfig, tunnel: &TunnelConfig) -> Option<String> {
    let configured = remote.public_url.trim().trim_end_matches('/');
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    if !tunnel.enabled || tunnel.config.is_empty() {
        return None;
    }
    let path = match (tunnel.config.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(&tunnel.config),
    };
    let yml = std::fs::read_to_string(path).ok()?;
    ingress_hostname(&yml).map(|h| format!("https://{}", h))
}

/// cloudflared の設定ファイルから最初の `hostname:` を取り出す
fn ingress_hostname(yml: &str) -> Option<String> {
    yml.lines()
        .map(|l| l.trim().trim_start_matches("- ").trim())
        .find_map(|l| l.strip_prefix("hostname:"))
        .map(|h| h.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|h| !h.is_empty())
}

/// QR コードのモジュール(true = 黒)。周囲の余白込みの正方形
pub fn qr_modules(text: &str) -> Result<Vec<Vec<bool>>> {
    let code = QrCode::new(text.as_bytes()).context("QR コードを作れません")?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + QUIET_ZONE * 2;
    let mut rows = vec![vec![false; size]; size];
    for y in 0..width {
        for x in 0..width {
            rows[y + QUIET_ZONE][x + QUIET_ZONE] = colors[y * width + x] == Color::Dark;
        }
    }
    Ok(rows)
}

/// QR コードの SVG(閲覧画面の「共有」で表示する)。黒の連続は 1 本の矩形にまとめて小さくする
pub fn qr_svg(text: &str) -> Result<String> {
    let m = qr_modules(text)?;
    let n = m.len();
    let mut rects = String::new();
    for (y, row) in m.iter().enumerate() {
        let mut x = 0;
        while x < n {
            if !row[x] {
                x += 1;
                continue;
            }
            let start = x;
            while x < n && row[x] {
                x += 1;
            }
            rects.push_str(&format!("M{} {}h{}v1h-{}z", start, y, x - start, x - start));
        }
    }
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {n} {n}\" shape-rendering=\"crispEdges\">\
         <rect width=\"{n}\" height=\"{n}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{rects}\"/></svg>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(public_url: &str) -> RemoteConfig {
        let mut r: RemoteConfig = toml::from_str("").unwrap();
        r.public_url = public_url.into();
        r
    }

    fn tunnel_with(config: &str) -> TunnelConfig {
        TunnelConfig {
            enabled: true,
            config: config.into(),
            ..TunnelConfig::default()
        }
    }

    #[test]
    fn hostname_from_cloudflared_yml() {
        let yml = "token: x\ningress:\n  - hostname: t.example.com\n    service: http://127.0.0.1:8765\n  - service: http_status:404\n";
        assert_eq!(ingress_hostname(yml).as_deref(), Some("t.example.com"));
        assert_eq!(ingress_hostname("ingress:\n  - hostname: \"q.example\"\n").as_deref(), Some("q.example"));
        assert_eq!(ingress_hostname("token: x\n"), None);
    }

    #[test]
    fn share_urls() {
        let dir = tempfile::tempdir().unwrap();
        let yml = dir.path().join("c.yml");
        std::fs::write(&yml, "ingress:\n  - hostname: t.example.com\n    service: http://x\n").unwrap();
        let t = tunnel_with(yml.to_str().unwrap());

        let s = ShareInfo::new(&remote(""), &t, "/?k=abc");
        assert_eq!(s.local, "http://127.0.0.1:8765/?k=abc");
        assert_eq!(s.public.as_deref(), Some("https://t.example.com/?k=abc"));

        // public_url の設定が優先(末尾の / は取る)
        let s = ShareInfo::new(&remote("https://other.example/"), &t, "/?k=abc");
        assert_eq!(s.public.as_deref(), Some("https://other.example/?k=abc"));

        // トンネル無し・トークンだけのトンネルでは公開 URL は分からない
        assert_eq!(ShareInfo::new(&remote(""), &TunnelConfig::default(), "/").public, None);
        let token_only = TunnelConfig { enabled: true, token: "t".into(), ..TunnelConfig::default() };
        assert_eq!(ShareInfo::new(&remote(""), &token_only, "/").public, None);
    }

    #[test]
    fn qr_has_quiet_zone_and_finder_pattern() {
        let m = qr_modules("https://t.example.com/?k=0123456789abcdef0123456789abcdef").unwrap();
        let n = m.len();
        assert!(m.iter().all(|r| r.len() == n));
        // 余白は白
        assert!(m[0].iter().all(|&d| !d) && m[n - 1].iter().all(|&d| !d));
        assert!(m.iter().all(|r| !r[0] && !r[n - 1]));
        // 左上の位置検出パターンの外枠(7x7 の黒い枠)
        let q = QUIET_ZONE;
        assert!((0..7).all(|i| m[q][q + i] && m[q + 6][q + i] && m[q + i][q] && m[q + i][q + 6]));
        // 端末の 80 桁に収まる大きさ
        assert!(n <= 60, "{}", n);
    }

    #[test]
    fn qr_decodes_back_to_url() {
        let url = "https://transcribe-private.bunfree.net/?k=0123456789abcdef0123456789abcdef";
        let m = qr_modules(url).unwrap();
        // 1 モジュール = 4x4 ピクセルの画像にして読み取る
        let scale = 4;
        let n = m.len();
        let mut img = rqrr::PreparedImage::prepare_from_greyscale(n * scale, n * scale, |x, y| {
            if m[y / scale][x / scale] { 0 } else { 255 }
        });
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, content) = grids[0].decode().unwrap();
        assert_eq!(content, url);
    }

    #[test]
    fn svg_has_one_path_for_dark_modules() {
        let svg = qr_svg("https://t.example.com/?k=abc").unwrap();
        let n = qr_modules("https://t.example.com/?k=abc").unwrap().len();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains(&format!("viewBox=\"0 0 {n} {n}\"")));
        assert_eq!(svg.matches("<path").count(), 1);
        assert!(svg.len() < 8_000, "{}", svg.len());
    }
}
