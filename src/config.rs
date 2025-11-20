use crate::types::DropPolicy;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub vad: VadConfig,
    #[serde(default)]
    pub buffer: BufferConfig,
    #[serde(default)]
    pub transcribe: TranscribeConfig,
    pub whisper: Option<WhisperConfig>,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub flac: FlacConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

/// オーディオ入力設定
///
/// オーディオデバイスからの入力に関する設定。
///
/// # デフォルト値
///
/// - `device_id`: "default" (システムのデフォルトデバイス)
/// - `sample_rate`: 16000 Hz (16kHz - AWS Transcribeの推奨値)
/// - `channels`: 4 (4チャンネル入力)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudioConfig {
    #[serde(default = "default_device_id")]
    pub device_id: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
}

/// VAD (Voice Activity Detection) 設定
///
/// 音声区間検出に関する設定。
///
/// # デフォルト値
///
/// - `threshold_db`: -40.0 dB
/// - `hangover_duration_ms`: 500 ms
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VadConfig {
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,
    #[serde(default = "default_hangover_duration_ms")]
    pub hangover_duration_ms: u32,
}

/// オーディオバッファ設定
///
/// リトライ用のバッファリングに関する設定。
///
/// # デフォルト値
///
/// - `capacity_seconds`: 300 秒
/// - `drop_policy`: DropOldest
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BufferConfig {
    #[serde(default = "default_capacity_seconds")]
    pub capacity_seconds: u32,
    #[serde(default = "default_drop_policy")]
    pub drop_policy: DropPolicy,
}

/// 文字起こしバックエンドの種類
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TranscribeBackendType {
    /// Amazon Transcribe
    Aws,
    /// OpenAI Whisper API
    Whisper,
}

/// AWS Transcribe 設定
///
/// AWS Transcribe Streaming APIに関する設定。
///
/// # デフォルト値
///
/// - `backend`: "aws" (Amazon Transcribe)
/// - `region`: "ap-northeast-1" (東京リージョン)
/// - `language_code`: "ja-JP" (日本語)
/// - `sample_rate`: 16000 Hz (16kHz)
/// - `max_retries`: 5 回
/// - `timeout_seconds`: 10 秒
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscribeConfig {
    #[serde(default = "default_backend")]
    pub backend: TranscribeBackendType,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default = "default_language_code")]
    pub language_code: String,
    #[serde(default = "default_transcribe_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

/// OpenAI Whisper API 設定
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WhisperConfig {
    /// OpenAI API Key
    pub api_key: String,
    /// Whisper モデル名（通常 "whisper-1"）
    #[serde(default = "default_whisper_model")]
    pub model: String,
    /// 言語コード（"ja", "en" など）。省略可能
    pub language: Option<String>,
    /// サンプルレート
    #[serde(default = "default_transcribe_sample_rate")]
    pub sample_rate: u32,
    /// 音声チャンクをためる時間（秒）
    #[serde(default = "default_chunk_duration_secs")]
    pub chunk_duration_secs: u64,
}

/// 出力設定
///
/// WAVファイル出力とログに関する設定。
///
/// # デフォルト値
///
/// - `wav_output_dir`: "./recordings"
/// - `log_level`: "info"
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutputConfig {
    #[serde(default = "default_wav_output_dir")]
    pub wav_output_dir: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

/// FLAC圧縮設定
///
/// Amazon Transcribeに送信する音声データのFLAC圧縮に関する設定。
///
/// # デフォルト値
///
/// - `compression_level`: 5 (バランス型、0-8の範囲)
/// - `enabled`: true (FLAC圧縮を使用)
///
/// # 圧縮レベル
///
/// - 0: 最速（圧縮率低）
/// - 5: バランス型（推奨）
/// - 8: 最高圧縮（処理時間長）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FlacConfig {
    #[serde(default = "default_flac_compression_level")]
    pub compression_level: u32,
    #[serde(default = "default_flac_enabled")]
    pub enabled: bool,
}

/// チャンネル個別設定
///
/// 各チャンネルの名前と有効/無効を設定。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelConfig {
    pub id: usize,
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

// Default functions
fn default_device_id() -> String {
    "default".to_string()
}

fn default_sample_rate() -> u32 {
    16000 // 16kHz - AWS Transcribeの推奨値
}

fn default_channels() -> u16 {
    4
}

fn default_threshold_db() -> f32 {
    -40.0
}

fn default_hangover_duration_ms() -> u32 {
    500
}

fn default_capacity_seconds() -> u32 {
    300
}

fn default_drop_policy() -> DropPolicy {
    DropPolicy::DropOldest
}

fn default_region() -> String {
    "ap-northeast-1".to_string()
}

fn default_language_code() -> String {
    "ja-JP".to_string()
}

fn default_transcribe_sample_rate() -> u32 {
    16000
}

fn default_max_retries() -> u32 {
    5
}

fn default_timeout_seconds() -> u64 {
    10
}

fn default_wav_output_dir() -> String {
    "./recordings".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_enabled() -> bool {
    true
}

fn default_flac_compression_level() -> u32 {
    5 // バランス型（推奨）
}

fn default_flac_enabled() -> bool {
    true // デフォルトでFLAC圧縮を使用
}

fn default_backend() -> TranscribeBackendType {
    TranscribeBackendType::Aws
}

fn default_whisper_model() -> String {
    "whisper-1".to_string()
}

fn default_chunk_duration_secs() -> u64 {
    5 // 5秒ごとにWhisper APIに送信
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio: AudioConfig::default(),
            vad: VadConfig::default(),
            buffer: BufferConfig::default(),
            transcribe: TranscribeConfig::default(),
            whisper: None, // デフォルトではWhisper設定なし
            output: OutputConfig::default(),
            flac: FlacConfig::default(),
            channels: vec![
                ChannelConfig {
                    id: 0,
                    name: "無線機1".to_string(),
                    enabled: true,
                },
                ChannelConfig {
                    id: 1,
                    name: "無線機2".to_string(),
                    enabled: true,
                },
            ],
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            device_id: default_device_id(),
            sample_rate: default_sample_rate(),
            channels: default_channels(),
        }
    }
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold_db: default_threshold_db(),
            hangover_duration_ms: default_hangover_duration_ms(),
        }
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self {
            capacity_seconds: default_capacity_seconds(),
            drop_policy: default_drop_policy(),
        }
    }
}

impl Default for TranscribeConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            region: default_region(),
            language_code: default_language_code(),
            sample_rate: default_transcribe_sample_rate(),
            max_retries: default_max_retries(),
            timeout_seconds: default_timeout_seconds(),
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            wav_output_dir: default_wav_output_dir(),
            log_level: default_log_level(),
        }
    }
}

impl Default for FlacConfig {
    fn default() -> Self {
        Self {
            compression_level: default_flac_compression_level(),
            enabled: default_flac_enabled(),
        }
    }
}

impl Config {
    /// 設定ファイルから読み込み
    ///
    /// TOML形式の設定ファイルをパースしてConfig構造体を生成する。
    ///
    /// # Arguments
    ///
    /// * `path` - 設定ファイルのパス
    ///
    /// # Errors
    ///
    /// ファイルの読み込みまたはパースに失敗した場合にエラーを返す。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use dcr_transcribe::config::Config;
    /// let config = Config::from_file("config.toml").unwrap();
    /// ```
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("設定ファイルの読み込みに失敗: {:?}", path.as_ref()))?;
        let config: Config =
            toml::from_str(&content).with_context(|| "設定ファイルのパースに失敗")?;
        Ok(config)
    }

    /// デフォルト設定をファイルに書き出し
    ///
    /// デフォルト値を持つ設定ファイルを生成する。
    /// 既存のファイルは上書きされる。
    ///
    /// # Arguments
    ///
    /// * `path` - 出力先のパス
    ///
    /// # Errors
    ///
    /// ファイルの書き込みに失敗した場合にエラーを返す。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use dcr_transcribe::config::Config;
    /// Config::write_default("config.toml").unwrap();
    /// ```
    pub fn write_default<P: AsRef<Path>>(path: P) -> Result<()> {
        let config = Config::default();
        let content =
            toml::to_string_pretty(&config).with_context(|| "設定のシリアライズに失敗")?;
        fs::write(path.as_ref(), content)
            .with_context(|| format!("設定ファイルの書き込みに失敗: {:?}", path.as_ref()))?;
        Ok(())
    }

    /// 設定ファイルがあれば読み込み、なければデフォルトを使用
    ///
    /// 設定ファイルの存在を確認し、存在する場合は読み込み、
    /// 存在しない場合はデフォルト設定を返す。
    ///
    /// # Arguments
    ///
    /// * `path` - 設定ファイルのパス
    ///
    /// # Errors
    ///
    /// ファイルが存在するがパースに失敗した場合にエラーを返す。
    /// ファイルが存在しない場合はエラーにならず、デフォルト設定を返す。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use dcr_transcribe::config::Config;
    /// let config = Config::load_or_default("config.toml").unwrap();
    /// ```
    pub fn load_or_default<P: AsRef<Path>>(path: P) -> Result<Self> {
        if path.as_ref().exists() {
            Self::from_file(path)
        } else {
            log::warn!(
                "設定ファイルが見つかりません。デフォルト設定を使用します: {:?}",
                path.as_ref()
            );
            Ok(Config::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.audio.sample_rate, 16000);
        assert_eq!(config.audio.channels, 4);
        assert_eq!(config.vad.threshold_db, -40.0);
        assert_eq!(config.buffer.capacity_seconds, 30);
        assert_eq!(config.transcribe.language_code, "ja-JP");
        assert_eq!(config.transcribe.region, "ap-northeast-1");
        assert_eq!(config.channels.len(), 2);
    }

    #[test]
    fn test_write_and_read_config() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        // デフォルト設定を書き込み
        Config::write_default(path).unwrap();

        // 読み込み
        let config = Config::from_file(path).unwrap();
        assert_eq!(config.audio.sample_rate, 16000);
        assert_eq!(config.transcribe.region, "ap-northeast-1");
    }

    #[test]
    fn test_custom_config() {
        let toml_content = r#"
[audio]
device_id = "test-device"
sample_rate = 16000
channels = 2

[vad]
threshold_db = -30.0
hangover_duration_ms = 1000

[buffer]
capacity_seconds = 60
drop_policy = "drop_newest"

[transcribe]
region = "us-east-1"
language_code = "en-US"
sample_rate = 16000
max_retries = 10
timeout_seconds = 20

[output]
wav_output_dir = "/tmp/test"
log_level = "debug"

[[channels]]
id = 0
name = "Channel 1"
enabled = true

[[channels]]
id = 1
name = "Channel 2"
enabled = false
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(toml_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = Config::from_file(temp_file.path()).unwrap();

        assert_eq!(config.audio.device_id, "test-device");
        assert_eq!(config.audio.sample_rate, 16000);
        assert_eq!(config.audio.channels, 2);
        assert_eq!(config.vad.threshold_db, -30.0);
        assert_eq!(config.vad.hangover_duration_ms, 1000);
        assert_eq!(config.buffer.capacity_seconds, 60);
        assert_eq!(config.buffer.drop_policy, DropPolicy::DropNewest);
        assert_eq!(config.transcribe.region, "us-east-1");
        assert_eq!(config.transcribe.language_code, "en-US");
        assert_eq!(config.transcribe.max_retries, 10);
        assert_eq!(config.output.wav_output_dir, "/tmp/test");
        assert_eq!(config.output.log_level, "debug");
        assert_eq!(config.channels.len(), 2);
        assert_eq!(config.channels[0].name, "Channel 1");
        assert!(config.channels[0].enabled);
        assert!(!config.channels[1].enabled);
    }

    #[test]
    fn test_load_or_default_nonexistent() {
        let config = Config::load_or_default("nonexistent_file.toml").unwrap();
        // デフォルト設定が返されることを確認
        assert_eq!(config.audio.sample_rate, 16000);
    }

    #[test]
    fn test_partial_config() {
        // 一部の設定のみ記述した場合、残りはデフォルト値が使われる
        let toml_content = r#"
[audio]
sample_rate = 32000

[[channels]]
id = 0
name = "Test Channel"
"#;

        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(toml_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let config = Config::from_file(temp_file.path()).unwrap();

        // 指定した値
        assert_eq!(config.audio.sample_rate, 32000);

        // デフォルト値
        assert_eq!(config.audio.device_id, "default");
        assert_eq!(config.audio.channels, 4);
        assert_eq!(config.vad.threshold_db, -40.0);
    }
}
