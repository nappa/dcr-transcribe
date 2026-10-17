pub use crate::aac::AacProfile;
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
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub segments: SegmentConfig,
    #[serde(default)]
    pub live: LiveConfig,
    #[serde(default)]
    pub tunnel: TunnelConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

/// リモート閲覧用 HTTP/SSE サーバ設定
///
/// `axum` ベースの軽量サーバを localhost で起動し、Cloudflare Tunnel 経由で公開する想定。
/// 確定結果のみリアルタイム配信(転送量最小化のため)。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteConfig {
    #[serde(default = "default_remote_enabled")]
    pub enabled: bool,
    /// listen アドレス。トンネル経由のみ想定なので 127.0.0.1 推奨
    #[serde(default = "default_remote_listen")]
    pub listen: String,
    /// 新規クライアントへ初期送信する履歴件数
    #[serde(default = "default_remote_history_size")]
    pub history_size: usize,
    /// partial 結果も配信するか(転送量増、デフォルト false)
    #[serde(default = "default_remote_include_partials")]
    pub include_partials: bool,
    /// partial を配信する場合のチャンネル毎の throttle 間隔(ms)
    #[serde(default = "default_remote_partial_throttle_ms")]
    pub partial_throttle_ms: u64,
    /// ログインキー。`/?k=<キー>` を開くとセッションクッキーが発行される。
    /// 空なら `login_key_file` のキーを使う
    #[serde(default)]
    pub login_key: String,
    /// ログインキーを保存するファイル。無ければ初回起動時に生成して保存する(再起動してもログイン URL が変わらない)。
    /// `login_key` も `login_key_file` も空ならログイン無し
    #[serde(default = "default_remote_login_key_file")]
    pub login_key_file: String,
    /// 廃止(Basic 認証・`?t=` トークン)。設定されていたら起動時にエラーにする(気付かず認証無しになるのを防ぐ)
    #[serde(default, skip_serializing)]
    pub auth_token: String,
    #[serde(default, skip_serializing)]
    pub basic_auth_user: String,
    #[serde(default, skip_serializing)]
    pub basic_auth_pass: String,
    /// アクセスを許可する国(ISO 3166 の 2 文字、例: ["JP"])。空なら制限なし。
    /// Cloudflare が付ける `CF-IPCountry` で判定する(ローカルからの直接アクセスは常に許可)
    #[serde(default)]
    pub allowed_countries: Vec<String>,
    /// Cloudflare Turnstile のサイトキー。シークレットと両方設定すると bot 確認を有効にする
    #[serde(default)]
    pub turnstile_site_key: String,
    /// Cloudflare Turnstile のシークレットキー
    #[serde(default)]
    pub turnstile_secret_key: String,
    /// bot 確認を通ったあと、再確認なしで使える時間(時間)
    #[serde(default = "default_remote_session_hours")]
    pub session_hours: u64,
}

fn default_remote_login_key_file() -> String {
    "./secrets/login_key".to_string()
}

fn default_remote_session_hours() -> u64 {
    12
}

/// 発話区間ごとの録音設定
///
/// 無音で区切った短い WAV をチャンネル毎に保存し、リモート閲覧画面から再生できるようにする。
/// 区間の開始・終了は VAD の閾値(`[vad] threshold_db`)で判定する。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SegmentConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 保存先。起動ごとにこの下へ日時名のディレクトリを作る
    #[serde(default = "default_segment_dir")]
    pub dir: String,
    /// 保存形式。aac (ADTS、配信向けで小さい) か wav
    #[serde(default)]
    pub format: SegmentFormat,
    /// AAC のビットレート(bps)
    #[serde(default = "default_segment_bitrate")]
    pub bitrate: u32,
    /// AAC のプロファイル。he (HE-AAC、低ビットレート向け) か lc
    #[serde(default)]
    pub profile: AacProfile,
    /// この長さ以上無音が続いたら区間を閉じる(ms)
    #[serde(default = "default_segment_silence_split_ms")]
    pub silence_split_ms: u64,
    /// 区間の頭に含める、発話直前の音声(ms)
    #[serde(default = "default_segment_pre_roll_ms")]
    pub pre_roll_ms: u64,
    /// 区間の末尾に残す無音(ms)
    #[serde(default = "default_segment_tail_ms")]
    pub tail_ms: u64,
    /// 発話がこれ未満の区間はノイズとして破棄(ms)
    #[serde(default = "default_segment_min_voice_ms")]
    pub min_voice_ms: u64,
    /// 1 区間の最大長(秒)。超えたら区切って次の区間に続ける
    #[serde(default = "default_segment_max_duration_secs")]
    pub max_duration_secs: u64,
}

/// cloudflared を本体の子プロセスとして管理する設定
///
/// 有効にすると、本体の起動と一緒に `cloudflared tunnel ... run` を立ち上げ、落ちたら起動し直し、
/// 本体の終了時に止める。`token` / `token_file`(ダッシュボードで作ったトンネル)か
/// `config`(ローカル管理のトンネル)のどれか 1 つを指定する。
/// cloudflared が見つからなければ公式リリースから `bin_dir` にダウンロードする。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TunnelConfig {
    #[serde(default)]
    pub enabled: bool,
    /// ダッシュボード(Zero Trust → Networks → Tunnels)で作ったトンネルのトークン
    #[serde(default)]
    pub token: String,
    /// cloudflared の実行ファイル(PATH・bin_dir から探す名前か、パス)
    #[serde(default = "default_tunnel_cloudflared")]
    pub cloudflared: String,
    /// cloudflared が見つからないとき、公式リリースからダウンロードするか
    #[serde(default = "default_tunnel_auto_download")]
    pub auto_download: bool,
    /// ダウンロードした cloudflared を置くディレクトリ
    #[serde(default = "default_tunnel_bin_dir")]
    pub bin_dir: String,
    /// cloudflared の設定ファイル(例: "~/.cloudflared/dcr-transcribe.yml")
    #[serde(default)]
    pub config: String,
    /// トンネルのトークンを書いたファイル(ダッシュボードで作ったトンネル用)
    #[serde(default)]
    pub token_file: String,
}

fn default_tunnel_cloudflared() -> String {
    "cloudflared".to_string()
}

fn default_tunnel_auto_download() -> bool {
    true
}

fn default_tunnel_bin_dir() -> String {
    "./bin".to_string()
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token: String::new(),
            cloudflared: default_tunnel_cloudflared(),
            auto_download: default_tunnel_auto_download(),
            bin_dir: default_tunnel_bin_dir(),
            config: String::new(),
            token_file: String::new(),
        }
    }
}

/// リモート閲覧画面からのライブ再生設定
///
/// 選ばれたチャンネルをサーバ側でミックスし、AAC (ADTS) で配信する。`[remote]` が有効なときだけ動く。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LiveConfig {
    #[serde(default)]
    pub enabled: bool,
    /// AAC のビットレート(bps)。聞いている間はこの帯域を使い続ける
    #[serde(default = "default_live_bitrate")]
    pub bitrate: u32,
    /// AAC のプロファイル。he (HE-AAC、低ビットレート向け) か lc
    #[serde(default)]
    pub profile: AacProfile,
}

fn default_live_bitrate() -> u32 {
    16000
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bitrate: default_live_bitrate(),
            profile: AacProfile::default(),
        }
    }
}

/// 区間録音の保存形式
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SegmentFormat {
    #[default]
    Aac,
    Wav,
}

fn default_segment_bitrate() -> u32 {
    24000
}

fn default_segment_dir() -> String {
    "./recordings/segments".to_string()
}

fn default_segment_silence_split_ms() -> u64 {
    2000
}

fn default_segment_pre_roll_ms() -> u64 {
    300
}

fn default_segment_tail_ms() -> u64 {
    300
}

fn default_segment_min_voice_ms() -> u64 {
    300
}

fn default_segment_max_duration_secs() -> u64 {
    300
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: default_segment_dir(),
            format: SegmentFormat::default(),
            bitrate: default_segment_bitrate(),
            profile: AacProfile::default(),
            silence_split_ms: default_segment_silence_split_ms(),
            pre_roll_ms: default_segment_pre_roll_ms(),
            tail_ms: default_segment_tail_ms(),
            min_voice_ms: default_segment_min_voice_ms(),
            max_duration_secs: default_segment_max_duration_secs(),
        }
    }
}

fn default_remote_enabled() -> bool {
    true
}

fn default_remote_listen() -> String {
    "127.0.0.1:8765".to_string()
}

fn default_remote_history_size() -> usize {
    200
}

fn default_remote_include_partials() -> bool {
    false
}

fn default_remote_partial_throttle_ms() -> u64 {
    500
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
/// - `output_device_id`: "default" (システムのデフォルト出力デバイス)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AudioConfig {
    #[serde(default = "default_device_id")]
    pub device_id: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    /// 音声出力デバイスID（TUIでチャンネル選択時に使用）
    #[serde(default = "default_device_id")]
    pub output_device_id: String,
}

/// VAD (Voice Activity Detection) 設定
///
/// 音声区間検出に関する設定。
///
/// # デフォルト値
///
/// - `threshold_db`: -40.0 dB
/// - `hangover_duration_ms`: 500 ms
/// - `silence_disconnect_threshold_ms`: 10000 ms (10秒)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VadConfig {
    #[serde(default = "default_threshold_db")]
    pub threshold_db: f32,
    #[serde(default = "default_hangover_duration_ms")]
    pub hangover_duration_ms: u32,
    /// 無音が何ミリ秒継続したらTranscribe APIへの接続を切断するか
    #[serde(default = "default_silence_disconnect_threshold_ms")]
    pub silence_disconnect_threshold_ms: u32,
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

/// 音声エンコード形式
///
/// Amazon Transcribeに送信する音声データのエンコード形式を指定する。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// FLAC形式（可逆圧縮、デフォルト）
    /// 圧縮率: 30-50%、処理負荷: 低
    #[default]
    Flac,
    /// OGG-Opus形式（非可逆圧縮）
    /// 圧縮率: 90%以上、処理負荷: 中
    Opus,
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
/// - `connect_on_startup`: false (音声検出まで接続しない)
/// - `send_buffered_on_reconnect`: true (再接続時にバッファを送信)
/// - `audio_format`: "flac" (FLAC形式)
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
    /// 起動時に即座にTranscribe APIに接続するか（false=音声検出まで待機）
    #[serde(default = "default_connect_on_startup")]
    pub connect_on_startup: bool,
    /// 再接続時に切断中に蓄積されたバッファの音声を送信するか
    #[serde(default = "default_send_buffered_on_reconnect")]
    pub send_buffered_on_reconnect: bool,
    /// 音声エンコード形式（flac または opus）
    #[serde(default)]
    pub audio_format: AudioFormat,
    /// Opusエンコードのビットレート（bps）
    /// 音声の場合は24000〜64000を推奨、高品質の場合は128000まで
    #[serde(default = "default_opus_bitrate")]
    pub opus_bitrate: i32,
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

fn default_silence_disconnect_threshold_ms() -> u32 {
    10000 // 10秒
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

fn default_connect_on_startup() -> bool {
    false // デフォルトでは音声検出まで接続しない
}

fn default_send_buffered_on_reconnect() -> bool {
    true // デフォルトでは再接続時にバッファを送信
}

fn default_opus_bitrate() -> i32 {
    64000 // 64kbps（音声に最適）
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
            remote: None, // デフォルトでは Remote サーバ無効
            segments: SegmentConfig::default(),
            live: LiveConfig::default(),
            tunnel: TunnelConfig::default(),

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
            output_device_id: default_device_id(),
        }
    }
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold_db: default_threshold_db(),
            hangover_duration_ms: default_hangover_duration_ms(),
            silence_disconnect_threshold_ms: default_silence_disconnect_threshold_ms(),
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
            connect_on_startup: default_connect_on_startup(),
            send_buffered_on_reconnect: default_send_buffered_on_reconnect(),
            audio_format: AudioFormat::default(),
            opus_bitrate: default_opus_bitrate(),
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
        assert_eq!(config.vad.silence_disconnect_threshold_ms, 10000);
        assert_eq!(config.buffer.capacity_seconds, 300);
        assert_eq!(config.transcribe.language_code, "ja-JP");
        assert_eq!(config.transcribe.region, "ap-northeast-1");
        assert_eq!(config.transcribe.connect_on_startup, false);
        assert_eq!(config.transcribe.send_buffered_on_reconnect, true);
        assert_eq!(config.transcribe.audio_format, AudioFormat::Flac);
        assert_eq!(config.channels.len(), 2);
    }

    #[test]
    fn test_write_and_read_config() {
        let temp_file = NamedTempFile::new().unwrap();
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
