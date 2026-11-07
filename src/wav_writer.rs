// 一部のメソッドは現時点で未使用だが API として保持する
#![allow(dead_code)]

use crate::types::{AudioChunk, CaptureTiming, SampleI16};
use anyhow::{Context, Result};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// チャンネル毎のWAVファイル書き出し
///
/// 無音区間を含む全音声データをWAVファイルとして保存
pub struct WavWriter {
    channel_id: usize,
    output_dir: PathBuf,
    current_file: Option<hound::WavWriter<BufWriter<fs::File>>>,
    spec: hound::WavSpec,
    samples_written: usize,
    max_file_samples: usize,
    samples_since_flush: usize,
    next_part: u64,
    current_path: Option<PathBuf>,
    timing_log: Option<BufWriter<fs::File>>,
    last_checkpoint: Option<usize>,
    previous_capture: Option<(CaptureTiming, u128, usize)>,
}

impl WavWriter {
    pub fn new<P: AsRef<Path>>(channel_id: usize, output_dir: P, sample_rate: u32) -> Result<Self> {
        anyhow::ensure!(
            sample_rate > 0,
            "録音の sample_rate は 0 より大きい値が必要です"
        );
        let output_dir = output_dir.as_ref().to_path_buf();

        // 出力ディレクトリが存在しない場合は作成
        if !output_dir.exists() {
            fs::create_dir_all(&output_dir)
                .with_context(|| format!("出力ディレクトリの作成に失敗: {:?}", output_dir))?;
        }

        let spec = hound::WavSpec {
            channels: 1, // モノラル（各チャンネル個別に保存）
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        Ok(Self {
            channel_id,
            output_dir,
            current_file: None,
            spec,
            samples_written: 0,
            max_file_samples: ((sample_rate as u64 * 3600).min((u32::MAX as u64 - 128) / 2))
                as usize,
            samples_since_flush: 0,
            next_part: 0,
            current_path: None,
            timing_log: None,
            last_checkpoint: None,
            previous_capture: None,
        })
    }

    /// WAVファイルを開始（新しいファイルを作成）
    pub fn start(&mut self) -> Result<()> {
        self.finalize()?;
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let (filepath, file) = loop {
            let part = self.next_part;
            self.next_part = self
                .next_part
                .checked_add(1)
                .context("録音ファイル番号の上限です")?;
            let suffix = if part == 0 {
                String::new()
            } else {
                format!("_{part:06}")
            };
            let path = self.output_dir.join(format!(
                "channel_{}_{}{}.wav",
                self.channel_id, timestamp, suffix
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => break (path, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("WAVファイルの作成に失敗: {:?}", path))
                }
            }
        };
        log::info!("WAVファイル作成: {:?}", filepath);
        let writer = hound::WavWriter::new(BufWriter::new(file), self.spec)
            .with_context(|| format!("WAVヘッダーの作成に失敗: {:?}", filepath))?;

        self.current_file = Some(writer);
        self.current_path = Some(filepath);
        self.last_checkpoint = None;
        self.samples_written = 0;
        self.samples_since_flush = 0;

        Ok(())
    }

    /// 音声位置と受信時計の対応を残す。時計は PC 由来で、同期精度は未検証。
    pub fn write_chunk(&mut self, chunk: &AudioChunk) -> Result<()> {
        anyhow::ensure!(
            chunk.format.sample_rate == self.spec.sample_rate && chunk.format.channels == 1,
            "録音フォーマットが一致しません"
        );
        let Some(capture) = chunk.capture else {
            return self.write_samples(&chunk.samples);
        };
        if chunk.samples.is_empty() {
            return Ok(());
        }
        let rate = self.spec.sample_rate as u128;
        let duration_ns = chunk.samples.len() as u128 * 1_000_000_000 / rate;
        let dropped;
        let mut stream_reset = false;
        let mut callback_gap = false;
        let mut clock_step = false;
        if let Some((prev, wall, count)) = self.previous_capture {
            let expected = prev.frame_index.saturating_add(count as u64);
            dropped = capture.frame_index.saturating_sub(expected);
            stream_reset =
                capture.frame_index < expected || capture.monotonic_ns < prev.monotonic_ns;
            let elapsed = capture.monotonic_ns.saturating_sub(prev.monotonic_ns);
            let expected_elapsed = (capture.frame_index.saturating_sub(prev.frame_index) as u128
                + chunk.samples.len() as u128)
                .saturating_sub(count as u128)
                * 1_000_000_000
                / rate;
            callback_gap = elapsed.abs_diff(expected_elapsed) >= 500_000_000;
            clock_step = ((chunk.timestamp_ns as i128 - wall as i128) - elapsed as i128)
                .unsigned_abs()
                >= 500_000_000;
        } else {
            dropped = capture.frame_index;
        }
        let mut offset = 0;
        while offset < chunk.samples.len() {
            if self.current_file.is_none() || self.samples_written >= self.max_file_samples {
                self.start()?;
            }
            if self.timing_log.is_none() {
                let path = self
                    .current_path
                    .as_ref()
                    .context("録音パスがありません")?
                    .with_extension("timing.jsonl");
                let file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .context("時刻記録ファイルの作成に失敗")?;
                self.timing_log = Some(BufWriter::new(file));
                self.timing_event(serde_json::json!({
                    "type": "header", "version": 1, "channel": self.channel_id,
                    "sample_rate": self.spec.sample_rate, "clock": "pc_system_time_unverified",
                    "time_basis": "callback_receipt_minus_chunk_duration",
                    "gap_policy": "omitted_audio_not_padded"
                }))?;
            }
            let count =
                (chunk.samples.len() - offset).min(self.max_file_samples - self.samples_written);
            let anomaly =
                offset == 0 && (dropped > 0 || stream_reset || callback_gap || clock_step);
            if self
                .last_checkpoint
                .is_none_or(|n| self.samples_written - n >= self.spec.sample_rate as usize)
                || anomaly
            {
                self.timing_event(serde_json::json!({
                    "type": "checkpoint", "wav_sample": self.samples_written,
                    "input_frame": capture.frame_index.saturating_add(offset as u64),
                    "estimated_unix_ns": chunk.timestamp_ns.saturating_sub(duration_ns)
                        .saturating_add(offset as u128 * 1_000_000_000 / rate).to_string(),
                    "received_unix_ns": chunk.timestamp_ns.to_string(),
                    "received_monotonic_ns": capture.monotonic_ns.to_string(),
                    "dropped_samples_before": if offset == 0 { dropped } else { 0 },
                    "stream_reset": offset == 0 && stream_reset,
                    "callback_timing_discontinuity": offset == 0 && callback_gap,
                    "pc_clock_step": offset == 0 && clock_step,
                    "synthetic_silence": capture.synthetic
                }))?;
                self.last_checkpoint = Some(self.samples_written);
            }
            self.write_samples(&chunk.samples[offset..offset + count])?;
            offset += count;
        }
        self.previous_capture = Some((capture, chunk.timestamp_ns, chunk.samples.len()));
        Ok(())
    }

    /// 入力品質の発報・解除を録音時刻に紐付ける。判定確定時点を記録する。
    pub fn record_input_quality(
        &mut self,
        timestamp_ns: u128,
        warning: Option<&str>,
    ) -> Result<()> {
        self.timing_event(serde_json::json!({
            "type": "input_quality", "received_unix_ns": timestamp_ns.to_string(),
            "wav_sample": self.samples_written, "warning": warning
        }))
    }

    fn timing_event(&mut self, value: serde_json::Value) -> Result<()> {
        if let Some(log) = &mut self.timing_log {
            serde_json::to_writer(&mut *log, &value).context("時刻記録の書き込みに失敗")?;
            log.write_all(b"\n")?;
            // 約1秒ごと。OS バッファへの書き出しであり電源断耐性の保証ではない。
            log.flush().context("時刻記録のフラッシュに失敗")?;
        }
        Ok(())
    }

    /// サンプルを書き込み
    pub fn write_samples(&mut self, samples: &[SampleI16]) -> Result<()> {
        let mut remaining = samples;
        while !remaining.is_empty() {
            if self.current_file.is_none() || self.samples_written >= self.max_file_samples {
                self.start()?;
            }
            let count = remaining
                .len()
                .min(self.max_file_samples - self.samples_written);
            if let Some(writer) = &mut self.current_file {
                for &sample in &remaining[..count] {
                    writer
                        .write_sample(sample)
                        .context("WAVファイルへのサンプル書き込みに失敗")?;
                }
                self.samples_written += count;
                self.samples_since_flush += count;
                // 途中停止しても、直近の確定済みヘッダーまで読めるようにする。
                if self.samples_since_flush as u64 >= self.spec.sample_rate as u64 * 10 {
                    writer.flush().context("WAVファイルのフラッシュに失敗")?;
                    self.samples_since_flush = 0;
                }
            }
            remaining = &remaining[count..];
        }

        Ok(())
    }

    /// 現在のファイルを終了
    pub fn finalize(&mut self) -> Result<()> {
        if let Some(writer) = self.current_file.take() {
            writer
                .finalize()
                .with_context(|| "WAVファイルのファイナライズに失敗")?;
            log::info!(
                "WAVファイル書き込み完了: チャンネル {}, {}サンプル ({:.2}秒)",
                self.channel_id,
                self.samples_written,
                self.samples_written as f64 / self.spec.sample_rate as f64
            );
            self.timing_event(serde_json::json!({
                "type": "finalized", "samples": self.samples_written,
                "finalized_unix_ns": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default().as_nanos().to_string()
            }))?;
            self.timing_log = None;
            self.samples_written = 0;
        }
        Ok(())
    }

    /// 書き込んだサンプル数
    pub fn samples_written(&self) -> usize {
        self.samples_written
    }

    /// 書き込んだ時間（秒）
    pub fn duration_seconds(&self) -> f64 {
        self.samples_written as f64 / self.spec.sample_rate as f64
    }
}

impl Drop for WavWriter {
    fn drop(&mut self) {
        if self.current_file.is_some() {
            if let Err(e) = self.finalize() {
                log::error!("WavWriter のドロップ時にエラー: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_wav_writer_basic() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let mut writer = WavWriter::new(0, temp_dir.path(), 16000)?;

        writer.start()?;

        // サンプルデータを生成
        let samples: Vec<i16> = (0..16000)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();

        writer.write_samples(&samples)?;
        writer.finalize()?;

        // ファイルが作成されたことを確認
        let files: Vec<_> = fs::read_dir(temp_dir.path())?
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1);

        Ok(())
    }
    #[test]
    fn twelve_hour_recording_rotates_without_losing_samples() -> Result<()> {
        let dir = TempDir::new()?;
        // 1 Hz で 12 時間相当。実時間を待たず、同じ分割・書き込み経路を通す。
        let mut writer = WavWriter::new(0, dir.path(), 1)?;
        let samples: Vec<i16> = (0..43200).map(|n| (n % 30000) as i16).collect();
        writer.write_samples(&samples)?;
        writer.finalize()?;
        let mut files: Vec<_> = fs::read_dir(dir.path())?
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        assert_eq!(files.len(), 12);
        let mut decoded = Vec::new();
        for path in files {
            let reader = hound::WavReader::open(path)?;
            assert_eq!(reader.duration(), 3600);
            decoded.extend(
                reader
                    .into_samples::<i16>()
                    .collect::<hound::Result<Vec<_>>>()?,
            );
        }
        assert_eq!(decoded, samples);
        Ok(())
    }

    #[test]
    fn active_recording_has_readable_header_after_flush() -> Result<()> {
        let dir = TempDir::new()?;
        let mut writer = WavWriter::new(0, dir.path(), 1000)?;
        writer.write_samples(&vec![12; 10000])?;
        let path = fs::read_dir(dir.path())?.next().unwrap()?.path();
        let reader = hound::WavReader::open(path)?;
        assert_eq!(reader.duration(), 10000);
        assert!(reader.into_samples::<i16>().all(|s| s.unwrap() == 12));
        Ok(())
    }

    #[test]
    fn restarting_writer_never_overwrites_existing_audio() -> Result<()> {
        let dir = TempDir::new()?;
        for sample in [11, 22] {
            let mut writer = WavWriter::new(0, dir.path(), 1000)?;
            writer.write_samples(&[sample])?;
            writer.finalize()?;
        }
        let mut values = Vec::new();
        for entry in fs::read_dir(dir.path())? {
            values.extend(
                hound::WavReader::open(entry?.path())?
                    .into_samples::<i16>()
                    .collect::<hound::Result<Vec<_>>>()?,
            );
        }
        values.sort();
        assert_eq!(values, vec![11, 22]);
        Ok(())
    }

    fn timed_chunk(frame: u64, mono_ms: u128, wall_ms: u128, count: usize) -> AudioChunk {
        AudioChunk {
            samples: vec![12; count],
            format: crate::types::AudioFormat {
                sample_rate: 1000,
                channels: 1,
            },
            timestamp_ns: wall_ms * 1_000_000,
            capture: Some(CaptureTiming {
                frame_index: frame,
                monotonic_ns: mono_ms * 1_000_000,
                synthetic: false,
            }),
        }
    }

    fn timing_files(dir: &Path) -> Vec<Vec<serde_json::Value>> {
        let mut paths: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|p| {
                fs::read_to_string(p)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn timing_distinguishes_queue_loss_device_pause_and_clock_step() -> Result<()> {
        let dir = TempDir::new()?;
        let mut writer = WavWriter::new(2, dir.path(), 1000)?;
        writer.write_chunk(&timed_chunk(0, 100, 10000, 100))?;
        writer.write_chunk(&timed_chunk(300, 400, 10300, 100))?; // 200 lost samples
        writer.write_chunk(&timed_chunk(400, 2500, 12400, 100))?; // callbacks stopped 2 sec
        writer.write_chunk(&timed_chunk(500, 2600, 11400, 100))?; // wall clock went backwards
        writer.finalize()?;
        let logs = timing_files(dir.path());
        let events = &logs[0];
        assert_eq!(events[1]["estimated_unix_ns"], "9900000000");
        assert_eq!(events[2]["wav_sample"], 100);
        assert_eq!(events[2]["dropped_samples_before"], 200);
        assert_eq!(events[2]["callback_timing_discontinuity"], false);
        assert_eq!(events[3]["callback_timing_discontinuity"], true);
        assert_eq!(events[3]["pc_clock_step"], false);
        assert_eq!(events[4]["pc_clock_step"], true);
        assert_eq!(events.last().unwrap()["samples"], 400);
        Ok(())
    }

    #[test]
    fn timing_follows_rotation_inside_a_chunk() -> Result<()> {
        let dir = TempDir::new()?;
        let mut writer = WavWriter::new(0, dir.path(), 1000)?;
        writer.max_file_samples = 150;
        writer.write_chunk(&timed_chunk(0, 100, 10000, 100))?;
        writer.write_chunk(&timed_chunk(100, 200, 10100, 100))?;
        writer.finalize()?;
        let logs = timing_files(dir.path());
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].last().unwrap()["samples"], 150);
        assert_eq!(logs[1][1]["wav_sample"], 0);
        assert_eq!(logs[1][1]["input_frame"], 150);
        assert_eq!(logs[1][1]["estimated_unix_ns"], "10050000000");
        assert_eq!(logs[1].last().unwrap()["samples"], 50);
        Ok(())
    }

    #[test]
    fn input_quality_events_preserve_audio_and_record_recovery() -> Result<()> {
        let dir = TempDir::new()?;
        let mut writer = WavWriter::new(0, dir.path(), 1000)?;
        writer.write_chunk(&timed_chunk(0, 100, 10000, 100))?;
        writer.record_input_quality(10_000_000_000, Some("ハム成分が継続"))?;
        writer.write_chunk(&timed_chunk(100, 200, 10100, 100))?;
        writer.record_input_quality(10_100_000_000, None)?;
        writer.finalize()?;
        let logs = timing_files(dir.path());
        let events: Vec<_> = logs[0]
            .iter()
            .filter(|e| e["type"] == "input_quality")
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["wav_sample"], 100);
        assert_eq!(events[0]["received_unix_ns"], "10000000000");
        assert!(events[1]["warning"].is_null());
        let audio = fs::read_dir(dir.path())?
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "wav"))
            .unwrap();
        let samples = hound::WavReader::open(audio)?
            .into_samples::<i16>()
            .collect::<hound::Result<Vec<_>>>()?;
        assert_eq!(samples, vec![12; 200]);
        Ok(())
    }

    #[test]
    fn timing_failure_is_a_recording_error() -> Result<()> {
        let dir = TempDir::new()?;
        let mut writer = WavWriter::new(0, dir.path(), 1000)?;
        writer.start()?;
        fs::create_dir(
            writer
                .current_path
                .as_ref()
                .unwrap()
                .with_extension("timing.jsonl"),
        )?;
        assert!(writer
            .write_chunk(&timed_chunk(0, 100, 10000, 100))
            .is_err());
        assert_eq!(writer.samples_written(), 0);
        Ok(())
    }

    #[test]
    fn high_sample_rate_rotation_stays_below_riff_limit() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = WavWriter::new(0, dir.path(), 768000)?;
        assert!(writer.max_file_samples as u64 * 2 + 128 <= u32::MAX as u64);
        Ok(())
    }
}
