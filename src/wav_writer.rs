use crate::types::SampleI16;
use anyhow::{Context, Result};
use std::fs;
use std::io::BufWriter;
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
}

impl WavWriter {
    pub fn new<P: AsRef<Path>>(
        channel_id: usize,
        output_dir: P,
        sample_rate: u32,
    ) -> Result<Self> {
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
        })
    }

    /// WAVファイルを開始（新しいファイルを作成）
    pub fn start(&mut self) -> Result<()> {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("channel_{}_{}.wav", self.channel_id, timestamp);
        let filepath = self.output_dir.join(&filename);

        log::info!("WAVファイル作成: {:?}", filepath);

        let writer = hound::WavWriter::create(&filepath, self.spec)
            .with_context(|| format!("WAVファイルの作成に失敗: {:?}", filepath))?;

        self.current_file = Some(writer);
        self.samples_written = 0;

        Ok(())
    }

    /// サンプルを書き込み
    pub fn write_samples(&mut self, samples: &[SampleI16]) -> Result<()> {
        if self.current_file.is_none() {
            self.start()?;
        }

        if let Some(writer) = &mut self.current_file {
            for &sample in samples {
                writer
                    .write_sample(sample)
                    .with_context(|| "WAVファイルへのサンプル書き込みに失敗")?;
            }
            self.samples_written += samples.len();
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
}
