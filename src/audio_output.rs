use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream, StreamConfig};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// 音声出力デバイスマネージャ
pub struct AudioOutput {
    device: Device,
    sample_rate: u32,
    stream: Option<Stream>,
    audio_tx: Option<mpsc::Sender<Vec<i16>>>,
}

impl AudioOutput {
    /// 新しいAudioOutputを作成
    pub fn new(device_name: Option<&str>, sample_rate: u32) -> Result<Self> {
        let host = cpal::default_host();

        // デバイスを選択
        let device = if let Some(name) = device_name {
            // 指定されたデバイス名で検索
            host.output_devices()?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .ok_or_else(|| anyhow::anyhow!("出力デバイス '{}' が見つかりません", name))?
        } else {
            // デフォルトデバイスを使用
            host.default_output_device()
                .ok_or_else(|| anyhow::anyhow!("デフォルト出力デバイスが見つかりません"))?
        };

        log::info!("出力デバイス: {}", device.name()?);

        Ok(Self {
            device,
            sample_rate,
            stream: None,
            audio_tx: None,
        })
    }

    /// デバイス一覧を表示
    pub fn list_devices() -> Result<()> {
        let host = cpal::default_host();
        println!("=== 利用可能な出力デバイス ===");

        for (idx, device) in host.output_devices()?.enumerate() {
            let name = device.name()?;
            let is_default = host
                .default_output_device()
                .and_then(|d| d.name().ok())
                .map(|default_name| default_name == name)
                .unwrap_or(false);

            let marker = if is_default { " (デフォルト)" } else { "" };
            println!("{}. {}{}", idx, name, marker);

            // サポートされている設定を表示
            if let Ok(config) = device.default_output_config() {
                println!(
                    "   サンプルレート: {} Hz, チャンネル数: {}",
                    config.sample_rate().0,
                    config.channels()
                );
            }
        }

        Ok(())
    }

    /// 音声ストリームを開始
    pub fn start(&mut self) -> Result<mpsc::Sender<Vec<i16>>> {
        let config = StreamConfig {
            channels: 1,
            sample_rate: cpal::SampleRate(self.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        log::info!(
            "出力ストリーム開始: サンプルレート={}Hz, チャンネル={}",
            config.sample_rate.0,
            config.channels
        );

        // チャンネルを作成（大きめのバッファ）
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<i16>>(1024);

        // サンプルバッファを共有
        let sample_buffer: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::new()));
        let sample_buffer_clone = sample_buffer.clone();

        // バックグラウンドタスクで音声データを受信してバッファに追加
        tokio::spawn(async move {
            while let Some(samples) = audio_rx.recv().await {
                let mut buffer = sample_buffer_clone.lock().unwrap();
                buffer.extend_from_slice(&samples);
            }
        });

        // 出力ストリームを構築
        let stream = self
            .device
            .build_output_stream(
                &config,
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let mut buffer = sample_buffer.lock().unwrap();

                    if buffer.len() >= data.len() {
                        // バッファから必要なサンプル数を取り出し
                        data.copy_from_slice(&buffer[..data.len()]);
                        buffer.drain(..data.len());
                    } else {
                        // バッファが不足している場合、利用可能な分だけコピーして残りは無音
                        let available = buffer.len();
                        if available > 0 {
                            data[..available].copy_from_slice(&buffer[..]);
                            buffer.clear();
                        }
                        // 残りは無音で埋める
                        data[available..].fill(0);
                    }
                },
                move |err| {
                    log::error!("出力ストリームエラー: {}", err);
                },
                None,
            )
            .context("出力ストリームの構築に失敗")?;

        // ストリームを再生開始
        stream.play().context("ストリームの再生開始に失敗")?;

        self.stream = Some(stream);
        self.audio_tx = Some(audio_tx.clone());

        Ok(audio_tx)
    }

    /// 音声ストリームを停止
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            log::info!("出力ストリームを停止しました");
        }
        self.audio_tx = None;
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop();
    }
}
