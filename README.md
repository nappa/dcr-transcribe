# DCR Transcriber (dcr-transcribe)

デジタル簡易無線機(DCR)・IP無線機からの音声から無音信号を除いて Amazon Transcribe に送り、
結果を TUI に表示するシステム

## 人間様向けの説明

主に Claude Code にコード書いてもらいました

## 主な機能

- **マルチチャンネル対応**: 複数の無線機からの音声を同時処理
- **VAD (Voice Activity Detection)**: 無音区間を自動検出してコスト削減
- **自動リトライ**: ネットワーク断に対応したリトライ機構
- **完全録音**: 無音区間を含む全音声をWAVファイルとして保存
- **リアルタイム文字起こし**: AWS Transcribe連携
- **音声出力機能**: 選択したチャンネルの音声を別デバイスに出力
- **リアルタイムTUI**: ターミナルUIで各チャンネルの状態を可視化
  - 入力ボリューム（リアルタイム・ピーク）
  - VAD状態（無音・音声）
  - Transcribe接続状態
  - 文字起こし結果のリアルタイム表示
  - チャンネル選択と音声出力

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
cargo run --release -- --show-interfaces
```

利用可能な入力デバイスと出力デバイスの一覧が表示されます。
音声出力機能では、デフォルトの出力デバイスが使用されます。

### 4. 実行

```bash
cargo run --release
```

または設定ファイルを指定：

```bash
cargo run config.toml
```

### 5. 停止

TUI画面で `q` または `Esc` キーを押すと確認ダイアログが表示されます。`Y` キーで終了を確定すると安全に停止します。
`Ctrl+C` で確認なしで即座に停止することもできます。
録音中のWAVファイルは自動的に保存されます。

## TUI (Terminal User Interface)

実行中は以下の情報がリアルタイムで表示されます：

### 各チャンネルごとの表示

各チャンネルは縦方向に以下のように構成されています：

1. **文字起こし結果**（上部、メイン表示エリア）
   - 確定文字起こしテキストを時刻とともに表示（最大100件を保持）
   - 表示件数は画面の縦方向スペースに応じて自動調整
   - 時刻は HH:MM:SS フォーマット（秒単位）
   - 古い結果から順に表示され、最新の結果が下部に追加される（下からわき上がる形式）
   - 複数行にわたる長文の場合も自然に折り返される
   - **リアルタイム部分結果**（Partial Results）:
     - 黄色のタイムスタンプで表示
     - 安定性に応じてテキストの色が変化:
       - 暗灰色: 低安定性（変更される可能性が高い）
       - 灰色: 中安定性
       - 白色: 高安定性（ほぼ確定）
     - 斜体で表示され、確定結果と区別可能
     - 同じ行で更新されるため、発話がリアルタイムに追従

2. **ボリューム表示**（下部）
   - 現在の入力ボリューム（200msecごとに更新、シアンのバー）
   - VAD閾値が赤い縦線で表示
   - 範囲: -60dB ~ 0dB

3. **ステータス表示**（最下部）
   - **VAD状態**:
     - 灰色 = 無音
     - 青色 = 音声あり
   - **Transcribe状態**:
     - 緑色 = 正常接続
     - 赤色 = エラー
     - 灰色 = 無通信

### TUI操作

- `q` または `Esc`: 終了（確認ダイアログが表示されます）
  - `Y`: 終了を確定
  - `N` または `Esc`: キャンセル
- `Ctrl+C`: 確認なしで即座に終了
- `1`～`0`: 対応するチャンネルの音声を出力デバイスに送る（トグル）
  - 選択されたチャンネルは黄色の枠で表示され、タイトルに `[出力中]` が表示されます
  - 同じ数字キーを再度押すと選択解除されます
  - 1つのチャンネルのみ選択可能です
- TUIは自動的に200msecごとに更新されます

## 設定ファイルの例

```toml
[audio]
device_id = "default"           # 入力デバイス（"default" または --show-interfaces で表示されたデバイス名）
output_device_id = "default"    # 出力デバイス（"default" または --show-interfaces で表示されたデバイス名）
sample_rate = 48000             # 48kHz
channels = 4                    # 入力チャンネル数

[vad]
threshold_db = -50.0
hangover_duration_ms = 500

[transcribe]
backend = "aws"                 # "aws" または "whisper"
region = "ap-northeast-1"       # 東京リージョン
language_code = "ja-JP"
sample_rate = 48000

[output]
wav_output_dir = "./recordings"
log_level = "info"

[[channels]]
id = 0
name = "無線機1"
enabled = true

[[channels]]
id = 1
name = "無線機2"
enabled = true
```

### 設定項目の説明

#### [audio] セクション
- `device_id`: 音声入力デバイス名（`--show-interfaces`で確認可能）
- `output_device_id`: 音声出力デバイス名（TUIでチャンネル選択時に使用）
- `sample_rate`: サンプリングレート（16000 Hzを推奨）
- `channels`: 入力チャンネル数

#### [transcribe] セクション
- `backend`: 文字起こしバックエンド（`"aws"` または `"whisper"`）
- AWS使用時は環境変数 `AWS_ACCESS_KEY_ID` と `AWS_SECRET_ACCESS_KEY` が必要
- Whisper使用時は `[whisper]` セクションで `api_key` を設定

#### [[channels]] セクション
- 各チャンネルの設定を複数定義可能
- `id`: チャンネルID（0から始まる連番）
- `name`: チャンネル名（TUI表示用）
- `enabled`: チャンネルの有効/無効

詳細は [ARCHITECTURE.md](ARCHITECTURE.md) を参照してください。

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
