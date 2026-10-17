//! 発話区間ごとの録音(AAC / WAV)
//!
//! 音量が閾値を超えた時点で区間を開始し、`silence_split_ms` 以上の無音が続いたら区間を閉じる。
//! VAD のハングオーバー(Transcribe 接続維持用で長め)とは独立に判定するので、
//! 発話の切れ目ごとに短いファイルができる。
//!
//! - 区間の頭は `pre_roll_ms` ぶん遡って保存(立ち上がりの欠け防止)
//! - 区間の末尾の無音は `tail_ms` だけ残して捨てる
//! - 発話が `min_voice_ms` 未満の区間はノイズとみなしてファイルごと削除
//! - `max_duration_secs` を超えたら区間を閉じて続きを新しい区間にする
//!
//! 無音中のサンプルは一旦メモリに保留し、発話が再開したらファイルへ書き出す。
//! これにより末尾の無音をストリーミング書き込みのまま切り詰められる。

use crate::aac::AacEncoder;
use crate::config::{SegmentConfig, SegmentFormat};
use crate::types::SampleI16;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 全チャンネル共通の区間 ID(プロセス内で単調増加)
static NEXT_SEGMENT_ID: AtomicU64 = AtomicU64::new(1);

/// 書き終えた録音区間
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentInfo {
    pub id: u64,
    pub channel: usize,
    /// 区間先頭の壁時計時刻(UNIX エポックからのミリ秒)
    pub start_unix_ms: u64,
    /// 区間の長さ(ミリ秒)
    pub duration_ms: u64,
    pub path: PathBuf,
}

/// 区間ファイルの書き出し先
enum SegmentWriter {
    Wav(hound::WavWriter<BufWriter<fs::File>>),
    Aac {
        encoder: AacEncoder,
        file: BufWriter<fs::File>,
        buf: Vec<u8>,
    },
}

impl SegmentWriter {
    fn create(path: &Path, spec: hound::WavSpec, format: SegmentFormat, bitrate: u32) -> Result<Self> {
        Ok(match format {
            SegmentFormat::Wav => Self::Wav(
                hound::WavWriter::create(path, spec)
                    .with_context(|| format!("区間 WAV の作成に失敗: {:?}", path))?,
            ),
            SegmentFormat::Aac => Self::Aac {
                encoder: AacEncoder::new(spec.sample_rate, bitrate)?,
                file: BufWriter::new(
                    fs::File::create(path)
                        .with_context(|| format!("区間 AAC の作成に失敗: {:?}", path))?,
                ),
                buf: Vec::new(),
            },
        })
    }

    fn write(&mut self, samples: &[SampleI16]) -> Result<()> {
        match self {
            Self::Wav(w) => {
                for &s in samples {
                    w.write_sample(s).context("区間 WAV への書き込みに失敗")?;
                }
            }
            Self::Aac { encoder, file, buf } => {
                buf.clear();
                encoder.encode(samples, buf)?;
                file.write_all(buf).context("区間 AAC への書き込みに失敗")?;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        match self {
            Self::Wav(w) => w.finalize().context("区間 WAV のファイナライズに失敗"),
            Self::Aac { mut encoder, mut file, mut buf } => {
                buf.clear();
                encoder.flush(&mut buf)?;
                file.write_all(&buf).context("区間 AAC への書き込みに失敗")?;
                file.flush().context("区間 AAC のフラッシュに失敗")
            }
        }
    }
}

/// 録音中の区間
struct OpenSegment {
    id: u64,
    start_unix_ms: u64,
    path: PathBuf,
    writer: SegmentWriter,
    /// ファイルに書いたサンプル数
    written: usize,
    /// 閾値を超えたサンプル数の累計(min_voice_ms 判定用)
    voice_samples: usize,
    /// 直近の発話後に続いている無音(未書き込み)
    pending_silence: Vec<SampleI16>,
}

pub struct SegmentRecorder {
    channel_id: usize,
    output_dir: PathBuf,
    spec: hound::WavSpec,
    format: SegmentFormat,
    bitrate: u32,
    threshold_db: f32,
    pre_roll_samples: usize,
    tail_samples: usize,
    silence_split_samples: usize,
    min_voice_samples: usize,
    max_samples: usize,
    /// 区間外の直近サンプル(pre-roll 用リングバッファ)
    pre_roll: VecDeque<SampleI16>,
    current: Option<OpenSegment>,
    /// 直近に開始した区間の ID(文字起こし結果のひも付け用)
    last_segment_id: Option<u64>,
}

impl SegmentRecorder {
    /// `output_dir` 直下にファイルを作る(呼び出し側でセッション単位のディレクトリを渡す想定)
    pub fn new<P: AsRef<Path>>(
        channel_id: usize,
        output_dir: P,
        sample_rate: u32,
        threshold_db: f32,
        config: &SegmentConfig,
    ) -> Result<Self> {
        let output_dir = output_dir.as_ref().to_path_buf();
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("区間録音ディレクトリの作成に失敗: {:?}", output_dir))?;

        let ms_to_samples = |ms: u64| (ms * sample_rate as u64 / 1000) as usize;

        Ok(Self {
            channel_id,
            output_dir,
            spec: hound::WavSpec {
                channels: 1,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
            format: config.format,
            bitrate: config.bitrate,
            threshold_db,
            pre_roll_samples: ms_to_samples(config.pre_roll_ms),
            tail_samples: ms_to_samples(config.tail_ms),
            silence_split_samples: ms_to_samples(config.silence_split_ms).max(1),
            min_voice_samples: ms_to_samples(config.min_voice_ms),
            max_samples: ms_to_samples(config.max_duration_secs * 1000).max(1),
            pre_roll: VecDeque::new(),
            current: None,
            last_segment_id: None,
        })
    }

    /// 直近に開始した区間の ID(録音中なら現在の区間)
    pub fn last_segment_id(&self) -> Option<u64> {
        self.last_segment_id
    }

    /// 1 チャンク分のサンプルを処理する
    ///
    /// * `volume_db` - このチャンクの音量(VAD が計算したもの)
    /// * `timestamp_ns` - チャンク先頭の壁時計時刻(UNIX エポックからのナノ秒)
    ///
    /// 区間が閉じたら、その区間の情報を返す(短すぎて破棄した区間は返さない)。
    pub fn process(
        &mut self,
        samples: &[SampleI16],
        volume_db: f32,
        timestamp_ns: u128,
    ) -> Result<Option<SegmentInfo>> {
        if samples.is_empty() {
            return Ok(None);
        }
        let is_voice = volume_db > self.threshold_db;

        let Some(seg) = self.current.as_mut() else {
            if is_voice {
                self.open(samples, timestamp_ns)?;
            } else {
                self.push_pre_roll(samples);
            }
            return Ok(None);
        };

        if is_voice {
            // 発話再開: 保留していた無音ごと書き出す
            let pending = std::mem::take(&mut seg.pending_silence);
            Self::write(seg, &pending)?;
            Self::write(seg, samples)?;
            seg.voice_samples += samples.len();

            if seg.written >= self.max_samples {
                // 長すぎる区間は切って、続きは次のチャンクから新しい区間にする
                return self.close();
            }
            return Ok(None);
        }

        seg.pending_silence.extend_from_slice(samples);
        if seg.pending_silence.len() >= self.silence_split_samples {
            return self.close();
        }
        Ok(None)
    }

    /// 録音中の区間があれば閉じる(停止時用)
    pub fn finish(&mut self) -> Result<Option<SegmentInfo>> {
        self.close()
    }

    fn push_pre_roll(&mut self, samples: &[SampleI16]) {
        self.pre_roll.extend(samples.iter().copied());
        let excess = self.pre_roll.len().saturating_sub(self.pre_roll_samples);
        self.pre_roll.drain(..excess);
    }

    fn open(&mut self, samples: &[SampleI16], timestamp_ns: u128) -> Result<()> {
        let id = NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed);
        let pre_roll_ms =
            self.pre_roll.len() as u64 * 1000 / self.spec.sample_rate as u64;
        let start_unix_ms = ((timestamp_ns / 1_000_000) as u64).saturating_sub(pre_roll_ms);

        let local = chrono::DateTime::from_timestamp_millis(start_unix_ms as i64)
            .map(|dt| dt.with_timezone(&chrono::Local).format("%H%M%S_%3f").to_string())
            .unwrap_or_default();
        let ext = match self.format {
            SegmentFormat::Aac => "aac",
            SegmentFormat::Wav => "wav",
        };
        let path = self
            .output_dir
            .join(format!("ch{}_{}_{}.{}", self.channel_id, local, id, ext));
        let writer = SegmentWriter::create(&path, self.spec, self.format, self.bitrate)?;

        let mut seg = OpenSegment {
            id,
            start_unix_ms,
            path,
            writer,
            written: 0,
            voice_samples: samples.len(),
            pending_silence: Vec::new(),
        };
        let pre_roll: Vec<SampleI16> = self.pre_roll.drain(..).collect();
        Self::write(&mut seg, &pre_roll)?;
        Self::write(&mut seg, samples)?;

        log::debug!("チャンネル {}: 区間録音開始 #{}", self.channel_id, id);
        self.current = Some(seg);
        self.last_segment_id = Some(id);
        Ok(())
    }

    fn close(&mut self) -> Result<Option<SegmentInfo>> {
        let Some(mut seg) = self.current.take() else {
            return Ok(None);
        };

        let tail_len = seg.pending_silence.len().min(self.tail_samples);
        let tail: Vec<SampleI16> = seg.pending_silence[..tail_len].to_vec();
        // 区間外になる残りの無音は次の区間の pre-roll に回す
        let rest: Vec<SampleI16> = seg.pending_silence[tail_len..].to_vec();
        Self::write(&mut seg, &tail)?;
        self.push_pre_roll(&rest);

        let duration_ms = seg.written as u64 * 1000 / self.spec.sample_rate as u64;
        seg.writer
            .finish()
            .with_context(|| format!("区間ファイルの書き出しに失敗: {:?}", seg.path))?;

        if seg.voice_samples < self.min_voice_samples {
            log::debug!(
                "チャンネル {}: 区間 #{} は発話が短いため破棄",
                self.channel_id,
                seg.id
            );
            if let Err(e) = fs::remove_file(&seg.path) {
                log::warn!("区間ファイルの削除に失敗: {:?}: {}", seg.path, e);
            }
            return Ok(None);
        }

        log::info!(
            "チャンネル {}: 区間録音完了 #{} ({:.1}秒) {:?}",
            self.channel_id,
            seg.id,
            duration_ms as f64 / 1000.0,
            seg.path
        );
        Ok(Some(SegmentInfo {
            id: seg.id,
            channel: self.channel_id,
            start_unix_ms: seg.start_unix_ms,
            duration_ms,
            path: seg.path,
        }))
    }

    fn write(seg: &mut OpenSegment, samples: &[SampleI16]) -> Result<()> {
        seg.writer.write(samples)?;
        seg.written += samples.len();
        Ok(())
    }
}

impl Drop for SegmentRecorder {
    fn drop(&mut self) {
        if let Err(e) = self.close() {
            log::error!("SegmentRecorder のドロップ時にエラー: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 1 kHz サンプリングにして 1 サンプル = 1 ms で考えられるようにする
    const RATE: u32 = 1000;

    fn config() -> SegmentConfig {
        SegmentConfig {
            enabled: true,
            // サンプル単位で中身を確かめたいので基本は WAV。AAC は専用のテストで見る
            format: SegmentFormat::Wav,
            pre_roll_ms: 20,
            tail_ms: 10,
            silence_split_ms: 50,
            min_voice_ms: 30,
            max_duration_secs: 60,
            ..SegmentConfig::default()
        }
    }

    fn recorder(dir: &Path, cfg: &SegmentConfig) -> SegmentRecorder {
        SegmentRecorder::new(0, dir, RATE, -40.0, cfg).unwrap()
    }

    /// 10 ms チャンクを `n` 個流す。voice なら -10 dB、無音なら -100 dB として扱う
    fn feed(
        rec: &mut SegmentRecorder,
        n: usize,
        voice: bool,
        t_ms: &mut u64,
    ) -> Vec<SegmentInfo> {
        let mut out = Vec::new();
        for _ in 0..n {
            let samples = vec![if voice { 1000 } else { 0 }; 10];
            let db = if voice { -10.0 } else { -100.0 };
            if let Some(info) = rec.process(&samples, db, *t_ms as u128 * 1_000_000).unwrap() {
                out.push(info);
            }
            *t_ms += 10;
        }
        out
    }

    fn read_wav(path: &Path) -> Vec<i16> {
        hound::WavReader::open(path)
            .unwrap()
            .samples::<i16>()
            .map(|s| s.unwrap())
            .collect()
    }

    #[test]
    fn splits_on_silence_with_pre_roll_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder(dir.path(), &config());
        let mut t = 1_000_000;

        assert!(feed(&mut rec, 5, false, &mut t).is_empty());
        assert!(feed(&mut rec, 4, true, &mut t).is_empty());
        let segs = feed(&mut rec, 5, false, &mut t);

        assert_eq!(segs.len(), 1);
        let seg = &segs[0];
        // pre-roll 20 + 発話 40 + tail 10
        assert_eq!(seg.duration_ms, 70);
        assert_eq!(seg.start_unix_ms, 1_000_000 + 50 - 20);
        let samples = read_wav(&seg.path);
        assert_eq!(samples.len(), 70);
        assert!(samples[..20].iter().all(|&s| s == 0));
        assert!(samples[20..60].iter().all(|&s| s == 1000));
        assert!(samples[60..].iter().all(|&s| s == 0));
    }

    #[test]
    fn short_pause_stays_in_one_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder(dir.path(), &config());
        let mut t = 0;

        feed(&mut rec, 4, true, &mut t);
        assert!(feed(&mut rec, 3, false, &mut t).is_empty()); // 30 ms < 50 ms
        feed(&mut rec, 4, true, &mut t);
        let segs = feed(&mut rec, 5, false, &mut t);

        assert_eq!(segs.len(), 1);
        // 発話 40 + 間 30 + 発話 40 + tail 10 (先頭は pre-roll なし)
        assert_eq!(segs[0].duration_ms, 120);
    }

    #[test]
    fn short_blip_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder(dir.path(), &config());
        let mut t = 0;

        feed(&mut rec, 2, true, &mut t); // 20 ms < min_voice 30 ms
        assert!(feed(&mut rec, 5, false, &mut t).is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        // 破棄した区間でも ID は採番済み(その間の文字起こしのひも付け先)
        assert!(rec.last_segment_id().is_some());
    }

    #[test]
    fn long_segment_is_split_at_max_duration() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentConfig {
            max_duration_secs: 1,
            ..config()
        };
        let mut rec = recorder(dir.path(), &cfg);
        let mut t = 0;

        let segs = feed(&mut rec, 150, true, &mut t);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].duration_ms, 1000);
        let first_id = segs[0].id;

        let segs = feed(&mut rec, 5, false, &mut t);
        assert_eq!(segs.len(), 1);
        assert!(segs[0].id > first_id);
        assert_eq!(segs[0].duration_ms, 510);
    }

    #[test]
    fn aac_segment_is_decodable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SegmentConfig {
            format: SegmentFormat::Aac,
            ..config()
        };
        // AAC は 8 kHz 以上が必要なので実際のレートで作る
        let mut rec = SegmentRecorder::new(0, dir.path(), 48000, -40.0, &cfg).unwrap();
        let voice: Vec<i16> = (0..4800).map(|i| ((i as f32 * 0.06).sin() * 8000.0) as i16).collect();
        let silence = vec![0i16; 4800];
        for _ in 0..10 {
            rec.process(&voice, -10.0, 0).unwrap();
        }
        let seg = rec.process(&silence, -100.0, 0).unwrap().expect("区間が閉じる");

        assert_eq!(seg.path.extension().unwrap(), "aac");
        // 発話 1000 ms + tail 10 ms(先頭は pre-roll なし)
        assert_eq!(seg.duration_ms, 1010);
        let decoded = crate::aac::tests::decode(&fs::read(&seg.path).unwrap());
        assert!(decoded.len() >= 48000, "{}", decoded.len());
    }

    #[test]
    fn finish_closes_open_segment() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = recorder(dir.path(), &config());
        let mut t = 0;

        feed(&mut rec, 4, true, &mut t);
        let seg = rec.finish().unwrap().expect("区間が返る");
        assert_eq!(seg.duration_ms, 40);
        assert_eq!(read_wav(&seg.path).len(), 40);
        assert!(rec.finish().unwrap().is_none());
    }
}
