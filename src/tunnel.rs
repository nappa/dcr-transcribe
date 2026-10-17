//! cloudflared を子プロセスとして管理する
//!
//! 本体の起動と一緒に `cloudflared tunnel ... run` を立ち上げ、落ちたら間隔をあけて立ち上げ直し、
//! 本体の終了時に止める。cloudflared の出力は本体のログに流す。

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

pub struct Tunnel {
    stop_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Tunnel {
    /// cloudflared を起動して見張りを始める
    pub fn start(config: &TunnelConfig) -> Result<Self> {
        let args = build_args(config)?;
        let program = config.cloudflared.clone();
        log::info!("トンネル: {} {}", program, args.join(" "));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(program, args, stop_rx));
        Ok(Self { stop_tx, task })
    }

    /// cloudflared を止めて見張りを終える
    pub async fn stop(self) {
        let _ = self.stop_tx.send(true);
        let _ = self.task.await;
    }
}

/// `cloudflared` に渡す引数。設定ファイル指定とトークンファイル指定のどちらか
fn build_args(config: &TunnelConfig) -> Result<Vec<String>> {
    let mut args = vec!["tunnel".to_string(), "--no-autoupdate".to_string()];
    match (config.config.is_empty(), config.token_file.is_empty()) {
        (false, true) => {
            args.push("--config".into());
            args.push(expand_home(&config.config).to_string_lossy().into_owned());
            args.push("run".into());
        }
        (true, false) => {
            args.push("run".into());
            args.push("--token-file".into());
            args.push(expand_home(&config.token_file).to_string_lossy().into_owned());
        }
        (true, true) => bail!("[tunnel] には config か token_file のどちらかを指定してください"),
        (false, false) => bail!("[tunnel] の config と token_file は同時に指定できません"),
    }
    Ok(args)
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

async fn supervise(program: String, args: Vec<String>, mut stop_rx: watch::Receiver<bool>) {
    let mut backoff = RESTART_INITIAL;
    loop {
        let started = tokio::time::Instant::now();
        match spawn(&program, &args, stop_rx.clone()) {
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

fn spawn(program: &str, args: &[String], stop_rx: watch::Receiver<bool>) -> Result<Child> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 本体が異常終了しても子プロセスを残さない
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("{} の起動に失敗", program))?;
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

    fn cfg(config: &str, token_file: &str) -> TunnelConfig {
        TunnelConfig {
            enabled: true,
            cloudflared: "cloudflared".into(),
            config: config.into(),
            token_file: token_file.into(),
        }
    }

    #[test]
    fn args_for_config_file() {
        std::env::set_var("HOME", "/home/u");
        assert_eq!(
            build_args(&cfg("~/.cloudflared/x.yml", "")).unwrap(),
            ["tunnel", "--no-autoupdate", "--config", "/home/u/.cloudflared/x.yml", "run"]
        );
    }

    #[test]
    fn args_for_token_file() {
        assert_eq!(
            build_args(&cfg("", "/etc/tok")).unwrap(),
            ["tunnel", "--no-autoupdate", "run", "--token-file", "/etc/tok"]
        );
    }

    #[test]
    fn args_require_exactly_one_source() {
        assert!(build_args(&cfg("", "")).is_err());
        assert!(build_args(&cfg("a.yml", "tok")).is_err());
    }

    /// 偽の cloudflared(シェルスクリプト)を作る
    fn fake(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("fake-cloudflared");
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn restarts_after_exit() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let program = fake(dir.path(), &format!("echo x >> {}; exit 1", count.display()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(program, vec![], stop_rx));
        // 1 秒 + 2 秒の待ちを挟んで 3 回起動されるはず
        tokio::time::sleep(Duration::from_millis(3500)).await;
        let _ = stop_tx.send(true);
        task.await.unwrap();
        let runs = std::fs::read_to_string(&count).unwrap().lines().count();
        assert_eq!(runs, 3);
    }

    #[tokio::test]
    async fn stop_terminates_running_child() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let program = fake(dir.path(), &format!("echo $$ > {}; exec sleep 30", pidfile.display()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(supervise(program, vec![], stop_rx));
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
