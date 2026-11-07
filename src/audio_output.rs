use anyhow::{Context, Result};
use crate::audio_input::cpal_device_name;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// 音声出力デバイスマネージャ
pub struct AudioOutput {
    health: crate::audio_health::StreamHealth,
    muted: Arc<std::sync::atomic::AtomicBool>,
    device: Device,
    sample_rate: u32,
    stream: Option<Stream>,
    audio_tx: Option<mpsc::Sender<Vec<i16>>>,
}

impl AudioOutput {
    pub fn set_health(&mut self, health: crate::audio_health::StreamHealth) {
        self.health = health;
    }

    pub fn set_mute_flag(&mut self, flag: Arc<std::sync::atomic::AtomicBool>) { self.muted = flag; }

    /// 新しいAudioOutputを作成
    pub fn new(device_name: Option<&str>, sample_rate: u32) -> Result<Self> {
        let host = cpal::default_host();

        // デバイスを選択
        let device = if let Some(name) = device_name {
            // 指定されたデバイス名で検索
            host.output_devices()?
                .find(|d| cpal_device_name(d).map(|n| n == name).unwrap_or(false))
                .ok_or_else(|| anyhow::anyhow!("出力デバイス '{}' が見つかりません", name))?
        } else {
            // デフォルトデバイスを使用
            host.default_output_device()
                .ok_or_else(|| anyhow::anyhow!("デフォルト出力デバイスが見つかりません"))?
        };

        log::info!("出力デバイス: {}", cpal_device_name(&device)?);

        Ok(Self {
            health: Default::default(),
            muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            let name = cpal_device_name(&device)?;
            let is_default = host
                .default_output_device()
                .and_then(|d| cpal_device_name(&d).ok())
                .map(|default_name| default_name == name)
                .unwrap_or(false);

            let marker = if is_default { " (デフォルト)" } else { "" };
            println!("{}. {}{}", idx, name, marker);

            // サポートされている設定を表示
            if let Ok(config) = device.default_output_config() {
                println!(
                    "   サンプルレート: {} Hz, チャンネル数: {}",
                    config.sample_rate(),
                    config.channels()
                );
            }
        }

        Ok(())
    }

    /// 音声ストリームを開始
    pub fn start(&mut self) -> Result<mpsc::Sender<Vec<i16>>> {
        // デバイスのデフォルト設定を取得してサンプルフォーマットを確認
        let default_config = self
            .device
            .default_output_config()
            .context("デフォルト出力設定が取得できません")?;

        log::info!(
            "出力デバイス設定: {:?}, {}Hz, {}ch",
            default_config.sample_format(),
            default_config.sample_rate(),
            default_config.channels()
        );

        let config = StreamConfig {
            channels: 1,
            sample_rate: self.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        log::info!(
            "出力ストリーム開始: サンプルレート={}Hz, チャンネル={}",
            config.sample_rate,
            config.channels
        );

        // チャンネルを作成（大きめのバッファ）
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<i16>>(8);

        // デバイスのサンプルフォーマットに応じてストリームを構築
        let stream = match default_config.sample_format() {
            SampleFormat::F32 => self.build_stream::<f32>(config, audio_rx)?,
            SampleFormat::I16 => self.build_stream::<i16>(config, audio_rx)?,
            SampleFormat::U16 => self.build_stream::<u16>(config, audio_rx)?,
            _ => anyhow::bail!(
                "サポートされていないサンプルフォーマット: {:?}",
                default_config.sample_format()
            ),
        };

        // ストリームを再生開始
        self.health.start();
        if let Err(error) = stream.play() {
            self.health.stop();
            return Err(error).context("ストリームの再生開始に失敗");
        }

        self.stream = Some(stream);
        self.audio_tx = Some(audio_tx.clone());

        Ok(audio_tx)
    }

    /// 指定されたサンプルフォーマットで出力ストリームを構築
    fn build_stream<T>(
        &self,
        config: StreamConfig,
        mut audio_rx: mpsc::Receiver<Vec<i16>>,
    ) -> Result<Stream>
    where
        T: SizedSample + Sample + FromSample<f32> + Send + 'static,
    {
        // サンプルバッファを共有
        let sample_buffer: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let sample_buffer_clone = sample_buffer.clone();
        // 出力が止まっても、保持する音声は直近 1 秒まで。
        let capacity = self.sample_rate as usize;

        // バックグラウンドタスクで音声データを受信してバッファに追加
        tokio::spawn(async move {
            while let Some(samples) = audio_rx.recv().await {
                let mut buffer = lock_samples(&sample_buffer_clone);
                append_samples(&mut buffer, &samples, capacity);
            }
        });

        let muted = self.muted.clone();
        let health = self.health.clone();
        let error_health = self.health.clone();
        // 出力ストリームを構築
        let stream = self
            .device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    if !data.is_empty() {
                        health.heartbeat();
                    }
                    let mut buffer = lock_samples(&sample_buffer);
                    if muted.load(std::sync::atomic::Ordering::Relaxed) {
                        buffer.clear();
                        data.fill(Sample::EQUILIBRIUM);
                        return;
                    }

                    if buffer.len() >= data.len() {
                        // バッファから必要なサンプル数を取り出し
                        for (i, sample) in data.iter_mut().enumerate() {
                            *sample = Self::convert_sample::<T>(buffer[i]);
                        }
                        buffer.drain(..data.len());
                    } else {
                        // バッファが不足している場合、利用可能な分だけコピーして残りは無音
                        let available = buffer.len();
                        for i in 0..data.len() {
                            if i < available {
                                data[i] = Self::convert_sample::<T>(buffer[i]);
                            } else {
                                data[i] = Sample::EQUILIBRIUM;
                            }
                        }
                        if available > 0 {
                            buffer.clear();
                        }
                    }
                },
                move |err| {
                    error_health.report_error();
                    log::error!("出力ストリームエラー: {}", err);
                },
                None,
            )
            .context("出力ストリームの構築に失敗")?;

        Ok(stream)
    }

    /// i16サンプルを指定されたフォーマットに変換
    fn convert_sample<T: Sample + FromSample<f32>>(sample: i16) -> T {
        // i16を-1.0~1.0の範囲に正規化してから対象フォーマットに変換
        let normalized = sample as f32 / i16::MAX as f32;
        T::from_sample(normalized)
    }

    /// 音声ストリームを停止
    pub fn stop(&mut self) {
        self.health.stop();
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

// 音声は再取得できないが、破損したバッファを破棄すれば再生を再開できる。
fn lock_samples(buffer: &Mutex<VecDeque<i16>>) -> std::sync::MutexGuard<'_, VecDeque<i16>> {
    buffer.lock().unwrap_or_else(|error| {
        let mut samples = error.into_inner();
        samples.clear();
        buffer.clear_poison();
        samples
    })
}

fn append_samples(buffer: &mut VecDeque<i16>, samples: &[i16], capacity: usize) {
    let incoming = &samples[samples.len().saturating_sub(capacity)..];
    let keep = capacity.saturating_sub(incoming.len());
    buffer.drain(..buffer.len().saturating_sub(keep));
    buffer.extend(incoming.iter().copied());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_output_retains_only_recent_audio() {
        let mut buffer = VecDeque::new();
        for i in 0..100 { append_samples(&mut buffer, &[i; 3], 5); }
        assert_eq!(buffer.into_iter().collect::<Vec<_>>(), vec![98, 98, 99, 99, 99]);
    }

    #[test]
    fn oversized_output_chunk_keeps_tail() {
        let mut buffer = VecDeque::from(vec![1, 2]);
        append_samples(&mut buffer, &[3, 4, 5, 6], 2);
        assert_eq!(buffer, VecDeque::from(vec![5, 6]));
        append_samples(&mut buffer, &[], 2);
        assert_eq!(buffer, VecDeque::from(vec![5, 6]));
        append_samples(&mut buffer, &[7], 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn poisoned_output_buffer_is_cleared_and_reusable() {
        let buffer = Mutex::new(VecDeque::from(vec![1, 2]));
        let _ = std::panic::catch_unwind(|| { let _guard = buffer.lock().unwrap(); panic!("injected"); });
        assert!(lock_samples(&buffer).is_empty());
        assert!(!buffer.is_poisoned());
        append_samples(&mut lock_samples(&buffer), &[3], 5);
        assert_eq!(lock_samples(&buffer).pop_front(), Some(3));
    }
}
