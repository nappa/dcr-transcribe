//! cloudflared を子プロセスとして管理する
//!
//! 本体の起動と一緒に `cloudflared tunnel ... run` を立ち上げ、落ちたら間隔をあけて立ち上げ直し、
//! 本体の終了時に止める。cloudflared の出力は本体のログに流す。
//!
//! - トンネルの指定は、ダッシュボードで作ったトンネルのトークン(`token` / `token_file`)か、
//!   ローカル管理の設定ファイル(`config`)。トークンは引数ではなく環境変数 `TUNNEL_TOKEN` で渡す(`ps` に出さない)
//! - cloudflared が見つからなければ、Cloudflare 公式の GitHub リリース(最新版)から `bin_dir` にダウンロードする。
//!   macOS では Cloudflare の開発者署名が付いているかを `codesign` で確かめてから使う

use crate::config::TunnelConfig;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const RESTART_INITIAL: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);
/// これだけ動き続けたら、次に落ちたときの待ち時間を最初に戻す
const STABLE_RUN: Duration = Duration::from_secs(60);
/// 停止要求から強制終了までの猶予
const STOP_GRACE: Duration = Duration::from_secs(3);

/// 公式リリースの配布元(`<ベース>/<ファイル名>` が最新版へ転送される)
const RELEASE_BASE: &str = "https://github.com/cloudflare/cloudflared/releases/latest/download/";
/// macOS の署名に含まれるべき発行者
const CLOUDFLARE_SIGNER: &str = "Developer ID Application: Cloudflare";

/// cloudflared の起動内容
#[derive(Debug, Clone, PartialEq)]
struct Launch {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

/// cloudflared の実行ファイルの探し方
#[derive(Debug, Clone)]
struct Locator {
    /// 設定の `cloudflared`(名前なら PATH と bin_dir から探す、パスならそれを使う)
    configured: String,
    bin_dir: PathBuf,
    auto_download: bool,
    release_base: String,
    verify_signature: bool,
    /// PATH からも探すか(テストでは本物の cloudflared を拾わないよう false にする)
    search_path: bool,
}

pub struct Tunnel {
    stop_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Tunnel {
    /// cloudflared を起動して見張りを始める
    pub fn start(config: &TunnelConfig) -> Result<Self> {
        let launch = build_launch(config)?;
        let locator = Locator {
            configured: config.cloudflared.clone(),
            bin_dir: expand_home(&config.bin_dir),
            auto_download: config.auto_download,
            release_base: RELEASE_BASE.to_string(),
            verify_signature: true,
            search_path: true,
        };
        log::info!("トンネル: cloudflared {}", launch.args.join(" "));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(locator, launch, stop_rx));
        Ok(Self { stop_tx, task })
    }

    /// cloudflared を止めて見張りを終える
    pub async fn stop(self) {
        let _ = self.stop_tx.send(true);
        let _ = self.task.await;
    }
}

/// 起動内容を組み立てる。token / token_file / config のどれか 1 つ
fn build_launch(config: &TunnelConfig) -> Result<Launch> {
    let set = [!config.token.is_empty(), !config.token_file.is_empty(), !config.config.is_empty()];
    match set.iter().filter(|b| **b).count() {
        0 => bail!("[tunnel] には token・token_file・config のどれかを指定してください"),
        1 => {}
        _ => bail!("[tunnel] の token・token_file・config は 1 つだけ指定してください"),
    }
    let mut args = vec!["tunnel".to_string(), "--no-autoupdate".to_string()];
    let mut env = Vec::new();
    if !config.token.is_empty() {
        args.push("run".into());
        env.push(("TUNNEL_TOKEN".to_string(), config.token.trim().to_string()));
    } else if !config.token_file.is_empty() {
        args.push("run".into());
        args.push("--token-file".into());
        args.push(expand_home(&config.token_file).to_string_lossy().into_owned());
    } else {
        args.push("--config".into());
        args.push(expand_home(&config.config).to_string_lossy().into_owned());
        args.push("run".into());
    }
    Ok(Launch { args, env })
}

/// この OS / CPU 用の配布ファイル名
fn release_asset() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "cloudflared-darwin-arm64.tgz",
        ("macos", "x86_64") => "cloudflared-darwin-amd64.tgz",
        ("linux", "x86_64") => "cloudflared-linux-amd64",
        ("linux", "aarch64") => "cloudflared-linux-arm64",
        ("windows", "x86_64") => "cloudflared-windows-amd64.exe",
        (os, arch) => bail!("{} / {} 向けの cloudflared は自動ダウンロードできません", os, arch),
    })
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

impl Locator {
    /// 既にある cloudflared を探す
    fn find(&self) -> Option<PathBuf> {
        if self.configured.contains('/') || self.configured.contains('\\') {
            let p = expand_home(&self.configured);
            return p.is_file().then_some(p);
        }
        let name = if cfg!(windows) && !self.configured.ends_with(".exe") {
            format!("{}.exe", self.configured)
        } else {
            self.configured.clone()
        };
        let in_path = if self.search_path {
            std::env::var_os("PATH")
                .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(&name)).find(|p| p.is_file()))
        } else {
            None
        };
        in_path.or_else(|| Some(self.bin_dir.join(&name)).filter(|p| p.is_file()))
    }

    /// cloudflared の場所。無ければダウンロードする
    async fn resolve(&self) -> Result<PathBuf> {
        if let Some(p) = self.find() {
            return Ok(p);
        }
        if !self.auto_download || self.configured != "cloudflared" {
            bail!("cloudflared が見つかりません: {}", self.configured);
        }
        self.download().await
    }

    async fn download(&self) -> Result<PathBuf> {
        let asset = release_asset()?;
        let url = format!("{}{}", self.release_base, asset);
        log::info!("トンネル: cloudflared が無いのでダウンロードします: {}", url);
        let bytes = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("ダウンロードに失敗: {}", url))?
            .bytes()
            .await
            .context("ダウンロードに失敗")?;

        std::fs::create_dir_all(&self.bin_dir)
            .with_context(|| format!("{:?} を作れません", self.bin_dir))?;
        let dest = self.bin_dir.join(exe_name());
        let work = self.bin_dir.join(".cloudflared-download");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        let result = self.install(asset, &bytes, &work, &dest).await;
        let _ = std::fs::remove_dir_all(&work);
        result?;
        log::info!("トンネル: cloudflared を {:?} に置きました ({} MB)", dest, bytes.len() / 1_000_000);
        Ok(dest)
    }

    /// ダウンロードしたものを展開・確認して `dest` に置く
    async fn install(&self, asset: &str, bytes: &[u8], work: &std::path::Path, dest: &std::path::Path) -> Result<()> {
        let staged = if asset.ends_with(".tgz") {
            let archive = work.join(asset);
            std::fs::write(&archive, bytes)?;
            let status = Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(work)
                .status()
                .await
                .context("tar を実行できません")?;
            if !status.success() {
                bail!("{} を展開できません", asset);
            }
            work.join("cloudflared")
        } else {
            let p = work.join(exe_name());
            std::fs::write(&p, bytes)?;
            p
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        if self.verify_signature {
            verify_signature(&staged).await?;
        }
        std::fs::rename(&staged, dest).with_context(|| format!("{:?} に置けません", dest))?;
        Ok(())
    }
}

/// macOS: Cloudflare の開発者署名が付いていて改ざんされていないことを確かめる
#[cfg(target_os = "macos")]
async fn verify_signature(path: &std::path::Path) -> Result<()> {
    let out = Command::new("codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(path)
        .output()
        .await
        .context("codesign を実行できません")?;
    if !out.status.success() {
        bail!("cloudflared の署名を確認できません: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let info = Command::new("codesign").args(["-dv", "--verbose=2"]).arg(path).output().await?;
    let detail = String::from_utf8_lossy(&info.stderr);
    if !detail.contains(CLOUDFLARE_SIGNER) {
        bail!("cloudflared の署名が Cloudflare のものではありません");
    }
    Ok(())
}

/// macOS 以外は HTTPS(GitHub の公式リリース)を信頼する
#[cfg(not(target_os = "macos"))]
async fn verify_signature(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

async fn supervise(locator: Locator, launch: Launch, mut stop_rx: watch::Receiver<bool>) {
    let mut backoff = RESTART_INITIAL;
    loop {
        let started = tokio::time::Instant::now();
        let spawned = match locator.resolve().await {
            Ok(program) => spawn(&program, &launch, stop_rx.clone()),
            Err(e) => Err(e),
        };
        match spawned {
            Ok(mut child) => {
                tokio::select! {
                    status = child.wait() => {
                        let ran = started.elapsed();
                        log::warn!("トンネル: cloudflared が終了しました ({:?}, {:.0}秒動作)", status.ok(), ran.as_secs_f64());
                        if ran >= STABLE_RUN {
                            backoff = RESTART_INITIAL;
                        }
                    }
                    _ = stop_rx.changed() => {
                        terminate(&mut child).await;
                        log::info!("トンネル: cloudflared を停止しました");
                        return;
                    }
                }
            }
            Err(e) => log::error!("トンネル: cloudflared を起動できません: {:#}", e),
        }

        log::info!("トンネル: {:.0}秒後に cloudflared を起動し直します", backoff.as_secs_f64());
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop_rx.changed() => return,
        }
        backoff = (backoff * 2).min(RESTART_MAX);
    }
}

fn spawn(program: &std::path::Path, launch: &Launch, stop_rx: watch::Receiver<bool>) -> Result<Child> {
    let mut child = Command::new(program)
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 本体が異常終了しても子プロセスを残さない
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("{:?} の起動に失敗", program))?;
    if let Some(out) = child.stdout.take() {
        tokio::spawn(forward_log(BufReader::new(out), stop_rx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        tokio::spawn(forward_log(BufReader::new(err), stop_rx));
    }
    Ok(child)
}

/// cloudflared の出力(`2026-..Z INF ...` 形式)を本体のログへ。
/// 止めている最中の出力(正常終了でも ERR で "Connection terminated" が出る)は警告にしない
async fn forward_log<R: tokio::io::AsyncRead + Unpin>(reader: BufReader<R>, stop_rx: watch::Receiver<bool>) {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("Registered tunnel connection") {
            log::info!("トンネル: 接続しました: {}", line);
        } else if line.contains(" ERR ") && !*stop_rx.borrow() {
            log::warn!("cloudflared: {}", line);
        } else {
            log::debug!("cloudflared: {}", line);
        }
    }
}

/// SIGTERM で止め、猶予を過ぎたら強制終了する
async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn cfg(token: &str, token_file: &str, config: &str) -> TunnelConfig {
        TunnelConfig {
            enabled: true,
            token: token.into(),
            token_file: token_file.into(),
            config: config.into(),
            ..TunnelConfig::default()
        }
    }

    #[test]
    fn launch_with_token_uses_env_not_args() {
        let l = build_launch(&cfg(" eyJabc \n", "", "")).unwrap();
        assert_eq!(l.args, ["tunnel", "--no-autoupdate", "run"]);
        assert_eq!(l.env, [("TUNNEL_TOKEN".to_string(), "eyJabc".to_string())]);
    }

    #[test]
    fn launch_with_token_file() {
        let l = build_launch(&cfg("", "/etc/tok", "")).unwrap();
        assert_eq!(l.args, ["tunnel", "--no-autoupdate", "run", "--token-file", "/etc/tok"]);
        assert!(l.env.is_empty());
    }

    #[test]
    fn launch_with_config_file() {
        std::env::set_var("HOME", "/home/u");
        let l = build_launch(&cfg("", "", "~/.cloudflared/x.yml")).unwrap();
        assert_eq!(l.args, ["tunnel", "--no-autoupdate", "--config", "/home/u/.cloudflared/x.yml", "run"]);
    }

    #[test]
    fn launch_requires_exactly_one_source() {
        assert!(build_launch(&cfg("", "", "")).is_err());
        assert!(build_launch(&cfg("t", "", "a.yml")).is_err());
        assert!(build_launch(&cfg("t", "f", "")).is_err());
    }

    #[test]
    fn release_asset_exists_for_this_platform() {
        let a = release_asset().unwrap();
        assert!(a.starts_with("cloudflared-"));
    }

    fn locator(dir: &std::path::Path, configured: &str, base: &str) -> Locator {
        Locator {
            configured: configured.into(),
            bin_dir: dir.join("bin"),
            auto_download: true,
            release_base: base.into(),
            verify_signature: false,
            search_path: false,
        }
    }

    #[tokio::test]
    async fn finds_existing_binary_in_bin_dir_without_download() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin").join(exe_name()), "x").unwrap();
        // ダウンロード先(127.0.0.1:9)には繋がらないので、見つからなければエラーになる
        let loc = locator(dir.path(), "cloudflared", "http://127.0.0.1:9/");
        assert_eq!(loc.resolve().await.unwrap(), dir.path().join("bin").join(exe_name()));
        // 名前が cloudflared 以外なら、無くてもダウンロードしない
        let loc = locator(dir.path(), "other-name", "http://127.0.0.1:9/");
        assert!(loc.resolve().await.unwrap_err().to_string().contains("見つかりません"));
        // パス指定で無ければエラー(ダウンロードしない)
        let loc = locator(dir.path(), "/no/such/cloudflared", "http://127.0.0.1:9/");
        assert!(loc.resolve().await.unwrap_err().to_string().contains("見つかりません"));
    }

    #[tokio::test]
    async fn downloads_and_installs_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let asset = release_asset().unwrap();
        // 中身は偽の実行ファイル(tgz なら tar で固める)
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("cloudflared"), "#!/bin/sh\necho fake\n").unwrap();
        let body = if asset.ends_with(".tgz") {
            let tgz = dir.path().join("a.tgz");
            let ok = std::process::Command::new("tar")
                .arg("-czf").arg(&tgz).arg("-C").arg(&src).arg("cloudflared")
                .status().unwrap().success();
            assert!(ok);
            std::fs::read(&tgz).unwrap()
        } else {
            std::fs::read(src.join("cloudflared")).unwrap()
        };
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", format!("/{}", asset).as_str())
            .with_body(body)
            .create_async()
            .await;

        let loc = locator(dir.path(), "cloudflared", &format!("{}/", server.url()));
        let path = loc.resolve().await.unwrap();
        m.assert_async().await;
        assert_eq!(path, dir.path().join("bin").join(exe_name()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "#!/bin/sh\necho fake\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!dir.path().join("bin/.cloudflared-download").exists(), "作業ディレクトリは消す");
        // 2 回目はダウンロードしない(既にある)
        assert_eq!(loc.find(), Some(path));
    }

    /// 公式リリースから本物をダウンロードして署名を確かめる(約 20MB)。
    /// `cargo test -- --ignored real_cloudflared_download`
    #[tokio::test]
    #[ignore]
    async fn real_cloudflared_download() {
        let dir = tempfile::tempdir().unwrap();
        let loc = Locator {
            verify_signature: true,
            ..locator(dir.path(), "cloudflared", RELEASE_BASE)
        };
        let path = loc.resolve().await.unwrap();
        let out = std::process::Command::new(&path).arg("--version").output().unwrap();
        let version = String::from_utf8_lossy(&out.stdout);
        println!("{:?}: {}", path, version.trim());
        assert!(version.contains("cloudflared version"));
    }

    /// 偽の cloudflared(シェルスクリプト)を bin_dir に置いた Locator
    fn fake(dir: &std::path::Path, body: &str) -> Locator {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("fake-cloudflared");
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        locator(dir, path.to_str().unwrap(), "http://127.0.0.1:9/")
    }

    fn no_args() -> Launch {
        Launch { args: vec![], env: vec![("TUNNEL_TOKEN".into(), "secret-token".into())] }
    }

    #[tokio::test]
    async fn restarts_after_exit_and_passes_token_env() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let loc = fake(dir.path(), &format!("echo \"$TUNNEL_TOKEN\" >> {}; exit 1", count.display()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(loc, no_args(), stop_rx));
        // 1 秒 + 2 秒の待ちを挟んで 3 回起動されるはず
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let _ = stop_tx.send(true);
        task.await.unwrap();
        let runs: Vec<String> = std::fs::read_to_string(&count).unwrap().lines().map(String::from).collect();
        assert_eq!(runs, ["secret-token"; 3]);
    }

    #[tokio::test]
    async fn stop_terminates_running_child() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let loc = fake(dir.path(), &format!("echo $$ > {}; exec sleep 30", pidfile.display()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(loc, no_args(), stop_rx));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();

        let t = tokio::time::Instant::now();
        let _ = stop_tx.send(true);
        task.await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "SIGTERM ですぐ止まる");
        // プロセスが残っていない
        let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        assert!(!alive);
    }
}
