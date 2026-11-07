//! 疑似の会場: オーディオ機器や AWS なしで中継サーバに QUIC でつなぎ、文字起こしを数秒ごとに送る。
//! 中継サーバ(VPS)を立てたあとの動作確認用。
//!
//! ```bash
//! cargo run --example fake_venue -- 203.0.113.5:443 ./secrets/uplink
//! ```
//! 2 つめの引数は `--gen-uplink-pki` で作った ca.pem / venue.pem / venue.key のあるディレクトリ。

use dcr_transcribe::config::{ChannelConfig, UplinkConfig};
use dcr_transcribe::remote_server::ChannelStatus;
use dcr_transcribe::types::TranscriptResult;
use dcr_transcribe::uplink::{live_queue, Uplink};
use std::time::{Duration, SystemTime};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let server = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:8443".into());
    let dir = args.get(2).cloned().unwrap_or_else(|| "./secrets/uplink".into());
    let config = UplinkConfig {
        enabled: true,
        server,
        ca_file: format!("{}/ca.pem", dir),
        cert_file: format!("{}/venue.pem", dir),
        key_file: format!("{}/venue.key", dir),
        live: false,
        ..Default::default()
    };
    let channels = [
        ChannelConfig { id: 2, name: "本部".into(), enabled: true },
        ChannelConfig { id: 3, name: "救護".into(), enabled: true },
    ];
    let start = SystemTime::now();
    let up = Uplink::new(&config, &channels, start, 48000)?;
    let (_live_tx, live_rx) = live_queue();
    let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(up.clone().run(live_rx, stop_rx));

    let lines = ["こちら本部、感度良好", "救護班、第二ゲートへ移動願います", "了解、移動します", "現在異常なし"];
    for i in 0.. {
        let ch = if i % 2 == 0 { 2 } else { 3 };
        let r = TranscriptResult::new(ch, lines[i % lines.len()].to_string(), false, None, start);
        up.push_transcript(&r, None);
        up.push_status(&[
            ChannelStatus { channel: 2, level_db: -20.0, receiving: ch == 2, transcribe: 1 },
            ChannelStatus { channel: 3, level_db: -50.0, receiving: ch == 3, transcribe: 0 },
        ]);
        let (_, line) = up.status();
        println!("{} | {}", lines[i % lines.len()], line);
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    Ok(())
}
