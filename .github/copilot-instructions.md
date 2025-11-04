# Copilot Instructions for dcr-transcribe

## プロジェクト概要
- **dcr-transcribe** はマルチチャンネル無線音声をリアルタイムで Amazon Transcribe へ送り、文字起こし結果を標準出力・WAVファイルに出力する Rust 製システムです。
- 主要構成: `AudioInput` (音声入力) → `ChannelProcessor` (各チャンネル処理/VAD/バッファ/Transcribe) → `WavWriter` (録音)・`TranscribeClient` (AWS連携)
- 詳細な設計は `ARCHITECTURE.md` を参照。

## 主要ワークフロー
- 設定生成: `cargo run -- --generate-config` で `config.toml` を生成・編集。
- デバイス一覧: `cargo run -- --show-interfaces` で利用可能なオーディオデバイスを確認。
- 実行: `cargo run` または `cargo run config.toml`。
- 停止: Ctrl+C で安全に停止、録音ファイルは自動保存。
- AWS認証は環境変数で指定 (`AWS_ACCESS_KEY_ID` など)。

## ビルド・テスト
- 標準的な Rust ワークフロー (`cargo build`, `cargo test`, `cargo run`)。
- テストは各モジュールの `#[cfg(test)]` で実装。AWS連携テストは `#[ignore]` 付き。
- `.vscode/settings.json` で `rust-analyzer` の import 整理・フォーマットが有効。

## コーディング規約・設計パターン
- **設定**: `config.toml` の構造は `ARCHITECTURE.md`/`README.md`/`src/config.rs` 参照。デフォルト値・型は `src/config.rs` に明示。
- **音声処理**: 各チャンネルは独立スレッド/タスクで処理。VADはRMSベース、バッファはリングバッファ＋ドロップポリシー。
- **ファイル出力**: WAVファイルは `recordings/` 配下、`channel_{id}_{timestamp}.wav` 形式。
- **Transcribe連携**: `src/transcribe.rs` 参照。リトライ・バックオフ・バッファ再送を実装。
- **ログ**: `log`/`env_logger` 使用。ログレベルは設定ファイルで制御。
- **エラー処理**: `anyhow` で一貫したエラー伝播。致命的エラーはログ出力後に継続または安全停止。

## 重要ファイル・ディレクトリ
- `src/` ... 各モジュール (audio_input, channel_processor, vad, buffer, wav_writer, transcribe, flac_encoder, types, config)
- `config.toml` ... 設定ファイル例
- `ARCHITECTURE.md` ... 詳細な設計・データフロー・エラー戦略
- `README.md` ... 概要・使い方・設定例

## プロジェクト固有の注意点
- **デバイス名フィルタ**: MacBook/Webcam等の不要デバイスは `audio_input.rs` で除外ロジックあり。
- **サンプルレート変換なし**: 入力デバイスとTranscribeのサンプルレートは一致させること。
- **未実装部分**: Transcribe API連携はダミー実装あり。実装時は `src/transcribe.rs` のTODO参照。
- **拡張性**: 新規チャンネル追加は `config.toml` の `[[channels]]` で定義。

---

- 詳細な設計・例外処理・拡張方針は `ARCHITECTURE.md` を必ず参照。
- コード例・型定義は `src/types.rs` も活用。
- 質問や不明点は `README.md` の How to use/設定例も参照。
