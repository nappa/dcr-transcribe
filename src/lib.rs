//! dcr-transcribe - デジタル簡易無線機の音声文字起こしシステム
//!
//! このクレートは、マルチチャンネルのオーディオ入力から音声を受信し、
//! Amazon Transcribeを使用して文字起こしを行うシステムを提供します。
//!
//! # 主な機能
//!
//! - **マルチチャンネル音声入力**: ZOOMなどのオーディオインターフェースから複数チャンネルを同時処理
//! - **VAD (Voice Activity Detection)**: 無音区間を自動検出して処理を最適化
//! - **バッファリング**: ネットワーク断に備えた音声データの一時保存
//! - **WAVファイル出力**: 全音声データをチャンネル毎にWAVファイルとして保存
//! - **AWS Transcribe連携**: リアルタイム文字起こし（実装中）
//!
//! # アーキテクチャ
//!
//! ```text
//! [Audio Interface] → [AudioInput] → [ChannelProcessor (×N)]
//!                                           ↓
//!                                    ┌──────┴──────┐
//!                                    │             │
//!                                  [VAD]      [WavWriter]
//!                                    │             │
//!                                    ↓             ↓
//!                               [Transcribe]   [WAV Files]
//!                                    │
//!                                    ↓
//!                              [Transcripts]
//! ```
//!
//! # 使用例
//!
//! ```no_run
//! use dcr_transcribe::config::Config;
//!
//! // 設定ファイルを読み込み
//! let config = Config::load_or_default("config.toml").unwrap();
//!
//! // またはデフォルト設定を生成
//! Config::write_default("config.toml").unwrap();
//! ```

pub mod audio_input;
pub mod audio_output;
pub mod aws_transcribe;
pub mod buffer;
pub mod channel_processor;
pub mod config;
pub mod flac_encoder;
pub mod transcribe;
pub mod transcribe_backend;
pub mod tui_state;
pub mod types;
pub mod vad;
pub mod wav_writer;
pub mod whisper_api;
