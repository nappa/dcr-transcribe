use anyhow::{Context, Result};
use dcr_transcribe::{
    access, audio_input::AudioInput, audio_output::AudioOutput, config::Config, tui_state::TuiState,
};
use env_logger::Env;
use std::{
    fs::OpenOptions,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

/// ログファイルに書き込むためのWriter
struct LogWriter(Arc<Mutex<std::fs::File>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // コマンドライン引数をパース
    let args: Vec<String> = std::env::args().collect();

    // 中継サーバモード(VPS で動かす。ログは標準エラーへ出して systemd などに任せる)
    if args.len() > 1 && args[1] == "--relay" {
        env_logger::Builder::from_env(Env::default().default_filter_or("info"))
            .format_timestamp_millis()
            .init();
        let config_path = args.get(2).map(String::as_str).unwrap_or("relay.toml");
        let config = Config::load_or_default(config_path)?;
        let running = Arc::new(AtomicBool::new(true));
        let signal = running.clone();
        ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))?;
        return dcr_transcribe::relay::run(config, running).await;
    }

    // ログファイルを開く
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("dcr-transcribe.log")
        .context("ログファイルを開けませんでした")?;

    let log_writer = LogWriter(Arc::new(Mutex::new(log_file)));

    // ロガーを初期化（ファイルに出力）
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .filter_module("flacenc", log::LevelFilter::Off)
        .target(env_logger::Target::Pipe(Box::new(log_writer)))
        .init();

    // デバイス一覧表示モード
    if args.len() > 1 && args[1] == "--show-interfaces" {
        println!("=== 入力デバイス ===");
        AudioInput::list_devices()?;
        println!();
        println!("=== 出力デバイス ===");
        AudioOutput::list_devices()?;
        return Ok(());
    }

    // ログイン URL の表示(キーはログに残さないので、必要なときにこれで確かめる)
    if args.len() > 1 && args[1] == "--show-login-url" {
        let config_path = args.get(2).map(String::as_str).unwrap_or("config.toml");
        let config = Config::load_or_default(config_path)?;
        let remote = config
            .remote
            .unwrap_or_else(|| toml::from_str("").expect("RemoteConfig の既定値"));
        let key = access::load_or_create_login_key(&remote.login_key, &remote.login_key_file)?;
        if key.is_empty() {
            println!("ログインキーが設定されていません(login_key / login_key_file が空)");
        } else {
            println!("ローカル:   http://{}/#k={}", remote.listen, key);
            println!("公開 URL:   https://<トンネル・中継サーバのホスト名>/#k={}", key);
        }
        return Ok(());
    }

    // 会場 ⇄ 中継サーバの mTLS 用の私設 CA と証明書を作る
    if args.len() > 1 && args[1] == "--gen-uplink-pki" {
        let (Some(out), Some(names)) = (args.get(2), args.get(3)) else {
            eprintln!("使い方: dcr-transcribe --gen-uplink-pki <出力先> <中継サーバの IP か名前>[,...] [有効日数]");
            std::process::exit(2);
        };
        let names: Vec<String> = names.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let days: i64 = args.get(4).map(|d| d.parse()).transpose()?.unwrap_or(400);
        dcr_transcribe::pki::generate(std::path::Path::new(out), &names, days)?;
        println!("{} に作りました(有効 {} 日、SAN: {})", out, days, names.join(", "));
        println!("  中継サーバへ: ca.pem relay.pem relay.key");
        println!("  会場へ:       ca.pem venue.pem venue.key");
        println!("  ca.key はどちらにも置かず、オフラインで保管してください");
        return Ok(());
    }

    // 設定ファイル生成モード
    if args.len() > 1 && args[1] == "--generate-config" {
        let config_path = if args.len() > 2 {
            &args[2]
        } else {
            "config.toml"
        };
        Config::write_default(config_path)?;
        println!("設定ファイルを生成しました: {}", config_path);
        return Ok(());
    }

    // 設定ファイルのパス
    let config_path = if args.len() > 1 && !args[1].starts_with("--") {
        &args[1]
    } else {
        "config.toml"
    };

    // 設定を読み込み
    let config = Config::load_or_default(config_path)?;

    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || signal.store(false, Ordering::SeqCst))?;
    dcr_transcribe::engine::run(
        config,
        TuiState::new(),
        running,
        Arc::new(AtomicBool::new(false)),
        true,
    )
    .await
}
