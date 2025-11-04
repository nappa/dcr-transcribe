# DCR Transcriber (dcr-transcribe)

デジタル簡易無線機(DCR)・IP無線機からの音声から無音信号を除いて Amazon Transcribe に送り、
結果を標準出力に出力する

## 主な機能

- **マルチチャンネル対応**: 複数の無線機からの音声を同時処理
- **VAD (Voice Activity Detection)**: 無音区間を自動検出してコスト削減
- **自動リトライ**: ネットワーク断に対応したリトライ機構
- **完全録音**: 無音区間を含む全音声をWAVファイルとして保存
- **リアルタイム文字起こし**: AWS Transcribe連携（開発中）

## デフォルト設定

- **サンプリング周波数**: 16kHz (AWS Transcribeの推奨値)
- **量子化ビット数**: 16bit (PCM)
- **AWSリージョン**: ap-northeast-1 (東京)
- **言語**: 日本語 (ja-JP)

## How to use

### 1. 設定ファイルの生成

```bash
cargo run -- --generate-config
```

これにより `config.toml` が生成されます。必要に応じて編集してください。

### 2. AWS アクセスキーを設定

```bash
export AWS_ACCESS_KEY_ID="your-access-key"
export AWS_SECRET_ACCESS_KEY="your-secret-key"
export AWS_REGION="ap-northeast-1"
```

### 3. オーディオインターフェースの確認

```bash
cargo run -- --show-interfaces
```

利用可能なオーディオデバイスの一覧が表示されます。

### 4. 実行

```bash
cargo run
```

または設定ファイルを指定：

```bash
cargo run config.toml
```

### 5. 停止

Ctrl+C で安全に停止します。録音中のWAVファイルは自動的に保存されます。

## 設定ファイルの例

```toml
[audio]
device_id = "default"
sample_rate = 16000  # 16kHz
channels = 4

[vad]
threshold_db = -40.0
hangover_duration_ms = 500

[transcribe]
region = "ap-northeast-1"  # 東京リージョン
language_code = "ja-JP"
sample_rate = 16000

[output]
wav_output_dir = "./recordings"
log_level = "info"

[[channels]]
id = 0
name = "無線機1"
enabled = true
```

詳細は [ARCHITECTURE.md](ARCHITECTURE.md) を参照してください。
