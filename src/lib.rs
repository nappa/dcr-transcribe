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
//! - **AWS Transcribe連携**: リアルタイム文字起こし
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

/// メモリアロケータは mimalloc の secure モード(ガードページ・空きリストのポインタの暗号化・割り当て位置のランダム化・
/// 二重解放の検出)。外から届く口(閲覧の HTTP・会場の QUIC)を持つので、ヒープを壊して乗っ取る攻撃を難しくする。
/// 速さ(FLAC のエンコード・音声処理)も保てる。バイナリと cdylib(SwiftUI アプリ)の両方がここで決まる
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 起動ログ用: アロケータの説明
pub fn allocator_desc() -> String {
    // mi_version() の値は版によって詰め方が違うので、読み解かずにそのまま出す
    format!("mimalloc (mi_version={}, secure)", mimalloc::MiMalloc.version())
}

#[cfg(test)]
mod allocator_tests {
    #[test]
    fn global_allocator_is_mimalloc() {
        let v: Vec<u8> = Vec::with_capacity(100);
        // mimalloc が確保した領域なら、mimalloc に大きさを問い合わせられる
        let usable = unsafe { mimalloc::MiMalloc.usable_size(v.as_ptr()) };
        assert!(usable >= 100, "{}", usable);
        assert!(super::allocator_desc().starts_with("mimalloc "));
    }
}

pub mod aac;
pub mod access;
pub mod audio_health;
pub mod input_quality;
pub mod audio_input;
pub mod audio_output;
pub mod aws_transcribe;
pub mod aws_settings;
pub mod browser;
pub mod buffer;
pub mod channel_processor;
pub mod clock;
pub mod config;
pub mod live;
pub mod flac_encoder;
pub mod opus_encoder;
pub mod pki;
pub mod private_relay;
pub mod relay;
pub mod remote_server;
pub mod security;
pub mod segment_recorder;
pub mod share;
pub mod transcribe;
pub mod transcribe_backend;
pub mod tui_state;
pub mod tunnel;
pub mod types;
pub mod uplink;
pub mod vad;
pub mod wav_writer;
pub mod whisper_api;

pub mod engine;
pub mod embedded;
pub mod tui;

pub mod audio_settings;
