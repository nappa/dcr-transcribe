use crate::config::AudioConfig;
use crate::types::{AudioChunk, AudioFormat};
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SizedSample};
use regex_lite::Regex;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// オーディオデバイスからのマルチチャンネル音声入力
pub struct AudioInput {
    device: cpal::Device,
    config: cpal::StreamConfig,
    stream: Option<cpal::Stream>,
    num_channels: u16,
}

impl AudioInput {
    /// 新しいAudioInputを作成
    pub fn new(config: &AudioConfig) -> Result<Self> {
        let host = cpal::default_host();

        log::info!("設定: {:?}", config);

        // デバイスを取得
        let device = if config.device_id == "default" {
            host.default_input_device()
                .context("デフォルト入力デバイスが見つかりません")?
        } else {
            // デバイスIDが指定されている場合は、デバイス一覧から検索
            Self::input_devices()?
                .into_iter()
                .find(|d| d.name().ok().as_deref() == Some(&config.device_id))
                .with_context(|| format!("デバイスが見つかりません: {}", config.device_id))?
        };

        log::info!("入力デバイス: {:?}", device.name());

        // デバイスの設定を取得
        let default_config = device
            .default_input_config()
            .context("デフォルト入力設定が取得できません")?;

        log::info!(
            "デバイス設定: {:?}, {}Hz, {}ch",
            default_config.sample_format(),
            default_config.sample_rate().0,
            default_config.channels()
        );

        // ストリーム設定を作成
        let stream_config = cpal::StreamConfig {
            channels: config.channels,
            sample_rate: cpal::SampleRate(config.sample_rate),
            buffer_size: cpal::BufferSize::Fixed(4096),
        };

        Ok(Self {
            device,
            config: stream_config,
            stream: None,
            num_channels: config.channels,
        })
    }

    /// ストリームを開始
    ///
    /// # Arguments
    /// * `channel_senders` - 各チャンネル用の送信チャンネル
    ///
    /// # Returns
    /// Result<()>
    pub fn start(&mut self, channel_senders: Vec<mpsc::Sender<AudioChunk>>) -> Result<()> {
        let num_channels = self.num_channels;
        let sample_rate = self.config.sample_rate.0;

        // デバイスのデフォルトフォーマットを取得
        let default_config = self.device.default_input_config()?;

        let stream = match default_config.sample_format() {
            cpal::SampleFormat::F32 => {
                self.build_stream::<f32>(channel_senders, num_channels, sample_rate)?
            }
            cpal::SampleFormat::I16 => {
                self.build_stream::<i16>(channel_senders, num_channels, sample_rate)?
            }
            cpal::SampleFormat::U16 => {
                self.build_stream::<u16>(channel_senders, num_channels, sample_rate)?
            }
            cpal::SampleFormat::I32 => {
                self.build_stream::<i32>(channel_senders, num_channels, sample_rate)?
            }
            _ => anyhow::bail!("サポートされていないサンプルフォーマット"),
        };

        stream.play().context("ストリームの再生開始に失敗")?;
        self.stream = Some(stream);

        log::info!("音声入力ストリームを開始しました");

        Ok(())
    }

    /// ストリームを構築
    fn build_stream<T>(
        &self,
        channel_senders: Vec<mpsc::Sender<AudioChunk>>,
        num_channels: u16,
        sample_rate: u32,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + Sample + Send + 'static,
        <T as Sample>::Float: Into<f32>,
    {
        let channel_senders = Arc::new(channel_senders);

        let data_callback = move |data: &[T], _info: &cpal::InputCallbackInfo| {
            // タイムスタンプを取得（全チャンネルで共有）
            let timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();

            // インターリーブされたデータを各チャンネルに分離
            let samples_per_channel = data.len() / num_channels as usize;

            // 各チャンネルを順次処理
            for ch in 0..num_channels as usize {
                if ch >= channel_senders.len() {
                    break;
                }

                // このチャンネルのサンプルを抽出
                let mut channel_samples = Vec::with_capacity(samples_per_channel);
                for frame in 0..samples_per_channel {
                    let idx = frame * num_channels as usize + ch;
                    if idx < data.len() {
                        let sample = data[idx];
                        let f = sample.to_float_sample().into();
                        let clamped = f.clamp(-1.0, 1.0);
                        let i16_sample = (clamped * i16::MAX as f32) as i16;
                        channel_samples.push(i16_sample);
                    }
                }

                // チャンクを作成
                let chunk = AudioChunk {
                    samples: channel_samples,
                    format: AudioFormat {
                        sample_rate,
                        channels: 1, // モノラル
                    },
                    timestamp_ns,
                };

                // 非同期送信（ブロッキングしない）
                if let Some(sender) = channel_senders.get(ch) {
                    match sender.try_send(chunk) {
                        Ok(_) => {
                            // 成功時はログ出力しない（パフォーマンス重視）
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            log::warn!("チャンネル {} への送信失敗: バッファ満杯", ch);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            log::warn!("チャンネル {} への送信失敗: チャンネルクローズ", ch);
                        }
                    }
                }
            }
        };

        let error_callback = move |err| {
            log::error!("ストリームエラー: {}", err);
        };

        let stream = self
            .device
            .build_input_stream(&self.config, data_callback, error_callback, None)
            .context("入力ストリームの構築に失敗")?;

        Ok(stream)
    }

    /// ストリームを停止
    pub fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            log::info!("音声入力ストリームを停止しました");
        }
    }

    /// デバイス一覧を表示
    pub fn list_devices() -> Result<()> {
        let host = cpal::default_host();
        println!("利用可能な入力デバイス:");
        println!();

        for (idx, device) in Self::input_devices()?.into_iter().enumerate() {
            let name = device.name()?;
            println!("  [{}] {}", idx, name);

            device.supported_input_configs()?.for_each(|config_range| {
                println!(
                    "      フォーマット: {:?}, {}-{}Hz, {}ch",
                    config_range.sample_format(),
                    config_range.min_sample_rate().0,
                    config_range.max_sample_rate().0,
                    config_range.channels()
                );
            });
            println!();
        }

        Ok(())
    }

    /// MacBook Air 本体・WebCam など、通常入力デバイスとして利用してはいけないデバイスを除外したデバイス一覧を取得
    fn input_devices() -> Result<Vec<cpal::Device>> {
        let host = cpal::default_host();
        let devices = host
            .input_devices()?
            .filter(|device| {
                if let Ok(name) = device.name() {
                    // 除外するデバイス名のリスト
                    let excluded_names_regex = Regex::new("MacBook (Air|Pro)|AirPods|iPhone|Webcam|Background|Microsoft Teams|ZoomAudioDevice").unwrap();
                    if excluded_names_regex.is_match(&name) {
                        return false;
                    }
                    return true;
                } else {
                    true
                }
            })
            .collect();
        Ok(devices)
    }
}

impl Drop for AudioInput {
    fn drop(&mut self) {
        self.stop();
    }
}
