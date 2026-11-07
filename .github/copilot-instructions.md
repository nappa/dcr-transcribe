# Copilot Instructions for dcr-transcribe

## プロジェクト概要
- 詳細な設計は `ARCHITECTURE.md` を参照。

## 主要ワークフロー
- 設定生成: `cargo run -- --generate-config` で `config.toml` を生成・編集。
- デバイス一覧: `cargo run -- --show-interfaces` で利用可能なオーディオデバイスを確認。
- 実行: `cargo run` または `cargo run config.toml`。
- 停止: Ctrl+C で安全に停止、録音ファイルは自動保存。

## ビルド・テスト
- 標準的な Rust ワークフロー (`cargo build`, `cargo test`, `cargo run`)。
- テストは各モジュールの `#[cfg(test)]` で実装。AWS連携テストは `#[ignore]` 付き。
- `.vscode/settings.json` で `rust-analyzer` の import 整理・フォーマットが有効。

## コーディング規約・設計パターン
- **設定**: `config.toml` の構造は `README.md`/`src/config.rs` 参照。デフォルト値・型は `src/config.rs` に明示。
- **音声処理**: 各チャンネルは独立スレッド/タスクで処理。VADはRMSベース、バッファはリングバッファ＋ドロップポリシー。
- **ファイル出力**: WAVファイルは `recordings/` 配下、`channel_{id}_{timestamp}.wav` 形式。
- **エラー処理**: `anyhow` で一貫したエラー伝播。致命的エラーはログ出力後に継続または安全停止。

## 重要ファイル・ディレクトリ
- `src/` ... 各モジュール (audio_input, channel_processor, vad, buffer, wav_writer, aws_transcribe, whisper_api, flac_encoder, types, config)
- `ARCHITECTURE.md` ... 概要・リモート閲覧の設計
- `README.md` ... 概要・使い方・設定例

## プロジェクト固有の注意点
- **デバイス名フィルタ**: MacBook/Webcam等の不要デバイスは `audio_input.rs` で除外ロジックあり。
- **サンプルレート変換なし**: 入力デバイスとTranscribeのサンプルレートは一致させること。
- **拡張性**: 新規チャンネル追加は `config.toml` の `[[channels]]` で定義。

---

- コード例・型定義は `src/types.rs` も活用。
- 質問や不明点は `README.md` の How to use/設定例も参照。
