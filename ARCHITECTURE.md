# dcr-transcribe アーキテクチャ設計書

## システム概要

dcr-transcribeは、デジタル簡易無線機（DCR）・IP無線機からの音声をリアルタイムで文字起こしするシステムです。
ZOOMオーディオインターフェースなどのマルチチャンネル入力デバイスから複数の無線機の音声を同時に受信し、
各チャンネルを独立して処理してAmazon Transcribeで文字起こしを行います。

### 主な特徴

- **マルチチャンネル対応**: 1つの入力デバイスから複数チャンネルの音声を独立処理
- **無音区間の最適化**: VAD（Voice Activity Detection）で無音区間を検出した場合も、Amazon Transcribeの仕様上、最低3分間は無音FLACデータを送り続ける。3分間完全無音が続いた場合はストリームを一旦切断し、音声再検出時に自動で再接続することでコスト削減。
- **ネットワーク耐性**: 5G回線の不安定性に対応したリトライ機構とバッファリング
- **完全な録音**: 無音区間を含めた全音声をチャンネル毎にWAVファイルとして保存
- **タイムスタンプ付き出力**: 秒単位のタイムスタンプで発話タイミングを記録
- **リアルタイムTUI**: ratatuiを使用したターミナルUIで各チャンネルの状態をリアルタイム表示
- **音声出力機能**: 選択したチャンネルの音声を別の出力デバイスにリアルタイム送信

## システム概要図

```
┌─────────────────────────────────────────────────────────────────┐
│                    ZOOM Audio Interface                         │
│                   (Multi-channel Input)                          │
└────┬────────┬────────┬────────┬─────────────────────────────────┘
     │        │        │        │
     │Ch1     │Ch2     │Ch3     │Ch4 ...
     │        │        │        │
     v        v        v        v
┌─────────────────────────────────────────────────────────────────┐
│                      AudioInput Module                           │
│  (cpal: デバイスからの音声ストリーム受信)                        │
└────┬────────┬────────┬────────┬─────────────────────────────────┘
     │        │        │        │
     v        v        v        v
┌─────────────────────────────────────────────────────────────────┐
│              ChannelProcessor (独立スレッド × N)                 │
│  ┌───────────────────────────────────────────────────┐          │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────────┐    │          │
│  │  │   VAD    │─>│  Buffer  │─>│ WavWriter    │    │          │
│  │  │ (無音検出)│  │(リトライ用)│  │(常時録音)    │    │          │
│  │  └────┬─────┘  └──────────┘  └──────────────┘    │          │
│  │       │音声区間のみ        ↓                      │          │
│  │       v                 TuiState (共有状態)       │          │
│  │  ┌──────────────────────────────┐                 │          │
│  │  │   TranscribeClient           │                 │          │
│  │  │ (リトライ機構・バッファ管理)  │                 │          │
│  │  └──────────┬───────────────────┘                 │          │
│  │             │文字起こし結果                        │          │
│  │             v                                     │          │
│  │  ┌──────────────────────────────┐                 │          │
│  │  │   OutputFormatter            │                 │          │
│  │  │ (タイムスタンプ付き出力)      │                 │          │
│  │  └──────────────────────────────┘                 │          │
│  │             │                                     │          │
│  │             │全サンプル (選択時のみ)               │          │
│  │             v                                     │          │
│  │  ┌──────────────────────────────┐                 │          │
│  │  │   AudioOutput (オプション)    │                 │          │
│  │  │ (選択チャンネルの音声出力)    │                 │          │
│  │  └──────────┬───────────────────┘                 │          │
│  └─────────────┼───────────────────────────────────┘          │
└────────────────┼────────────────────────────────────────────┘
                 │
        ┌────────┴─────────────┐
        v                      v
┌───────────────┐      ┌──────────────────┐
│   TUI Module  │      │ ログ出力         │
│   (ratatui)   │      │ WAVファイル      │
│ リアルタイム  │      │ 音声出力デバイス │
│ 状態表示      │      └──────────────────┘
│ チャンネル選択│
└───────────────┘
```

## データフロー

### 1. 音声入力フェーズ

```
[オーディオインターフェース]
    → [AudioInput: cpalストリームコールバック]
    → チャンネル分離
    → 各ChannelProcessorへ配信
```

- オーディオインターフェースからのサンプリングレート: 通常 44.1kHz または 48kHz
- サンプルフォーマット: i16 (16-bit PCM)
- 各チャンネルは独立したスレッドで処理

### 2. チャンネル処理フェーズ

各ChannelProcessorは以下の処理を並行実行：

#### a) VAD（Voice Activity Detection）
```
[音声サンプル]
    → パワー計算（RMS）
    → 閾値判定
    → 音声区間 / 無音区間の判定
```

**判定基準**:
- RMS（Root Mean Square）による音声パワー計算
- 設定可能な閾値（デフォルト: -40dB）
- ハングオーバー時間: 音声終了後も一定時間（例: 500ms）は音声区間とみなす

#### b) バッファリング
```
[音声サンプル]
    → リングバッファに追加
    → Transcribeへ送信
    → 送信成功後も一定期間保持（リトライ用）
```

**バッファ戦略**:
- 容量: 設定可能（デフォルト: 300秒分）
- ドロップポリシー: 最古のデータから破棄（DropOldest）
- リトライ時は保持済みバッファから再送

#### c) WAV書き出し
```
[全音声サンプル（無音含む）]
    → WavWriter
    → チャンネル毎のWAVファイル出力
```

**ファイル命名規則**:
- `channel_{channel_id}_{timestamp}.wav`
- 例: `channel_0_20250102_143000.wav`

### 3. Transcribe送信フェーズ

```
[音声区間]
    → TranscribeClient
    → AWS Transcribe Streaming API
    → 文字起こし結果受信
    → タイムスタンプ付きで出力
```

**送信仕様**:
- プロトコル: WebSocket over HTTPS
- エンコーディング: FLAC (Transcribe APIの仕様に準拠)
- チャンク単位: 設定可能（デフォルト: 100ms相当 or 128KB）
- 言語: ja-JP（設定ファイルで変更可能）
- VADで無音判定時も、最低3分間は無音FLACデータ（エンコーダで生成した無音チャンク）を送り続ける。
- 3分間完全無音が続いた場合はTranscribeストリームを一旦切断し、音声再検出時に自動で再接続する。
- ストリーム切断・再接続は自動で行われ、ユーザー操作不要。

### 4. 出力フェーズ

```
[Transcribe結果]
    → タイムスタンプ追加
    → チャンネル情報追加
    → 標準出力
```

### 5. 音声出力フェーズ（オプション）

```
[TUIで選択されたチャンネルの音声サンプル]
    → AudioOutput
    → cpal出力ストリーム
    → 出力デバイス（スピーカー等）
```

**音声出力仕様**:
- TUIで数字キー（0-9）を押すことで対応するチャンネルを選択
- 選択されたチャンネルの音声がリアルタイムで出力デバイスに送信
- 1つのチャンネルのみ選択可能（トグル動作）
- デフォルト出力デバイスを使用
- サンプルレート: 入力と同じ（通常16kHz）
- チャンネル数: モノラル（1ch）

**出力フォーマット**:
```json
{
  "channel": 0,
  "timestamp": "2025-01-02T14:30:15.234Z",
  "timestamp_seconds": 1704204615.234,
  "text": "こちら本部、応答願います",
  "is_partial": false
}
```

## コンポーネント設計

### AudioInput モジュール

**責務**: オーディオデバイスからのマルチチャンネル音声入力

**主要インターフェース**:
```rust
pub struct AudioInput {
    device: cpal::Device,
    config: cpal::StreamConfig,
    channels: Vec<mpsc::Sender<AudioChunk>>,
}

impl AudioInput {
    pub fn new(device_id: Option<String>) -> Result<Self>;
    pub fn start(&mut self) -> Result<()>;
    pub fn stop(&mut self);
}
```

**内部動作**:
- cpalを使用してデフォルトまたは指定されたデバイスを開く
- インターリーブされたマルチチャンネルデータを各チャンネルに分離
- 各チャンネル用のmpsc channelを通じてChannelProcessorに送信

### ChannelProcessor モジュール

**責務**: 1つのチャンネルの完全な処理パイプライン

**主要インターフェース**:
```rust
pub struct ChannelProcessor {
    channel_id: usize,
    vad: VoiceActivityDetector,
    buffer: AudioBuffer,
    transcribe_client: TranscribeClient,
    wav_writer: WavWriter,
    config: ChannelConfig,
}

impl ChannelProcessor {
    pub fn new(channel_id: usize, config: ChannelConfig) -> Self;
    pub async fn process_chunk(&mut self, chunk: AudioChunk) -> Result<()>;
}
```

**処理フロー**:
1. 音声チャンクを受信
2. WAVファイルに書き込み（無音含む全データ）
3. VADで音声区間を判定
4. 音声区間の場合、バッファに追加してTranscribeに送信
5. Transcribe結果を受信して出力

### VoiceActivityDetector (VAD) モジュール

**責務**: 音声区間の検出

**主要インターフェース**:
```rust
pub struct VoiceActivityDetector {
    threshold_db: f32,
    hangover_duration_ms: u32,
    state: VadState,
}

enum VadState {
    Silence,
    Voice { hangover_remaining_ms: u32 },
}

impl VoiceActivityDetector {
    pub fn new(threshold_db: f32, hangover_duration_ms: u32) -> Self;
    pub fn process(&mut self, samples: &[i16]) -> bool; // true = 音声あり
}
```

**アルゴリズム**:
- RMS（Root Mean Square）でパワーを計算
- デシベル変換: `20 * log10(rms / i16::MAX)`
- 閾値を超えたら音声開始
- 閾値を下回っても、ハングオーバー期間は音声継続とみなす

### AudioBuffer モジュール

**責務**: リトライ用の音声データバッファリング

**主要インターフェース**:
```rust
pub struct AudioBuffer {
    capacity_seconds: u32,
    samples: VecDeque<BufferedChunk>,
    drop_policy: DropPolicy,
}

struct BufferedChunk {
    samples: Vec<i16>,
    timestamp_ns: u128,
}

impl AudioBuffer {
    pub fn new(capacity_seconds: u32, drop_policy: DropPolicy) -> Self;
    pub fn push(&mut self, chunk: AudioChunk);
    pub fn get_range(&self, from_ns: u128, to_ns: u128) -> Vec<i16>;
    pub fn clear_before(&mut self, timestamp_ns: u128);
}
```

**バッファ管理**:
- リングバッファとして実装
- 容量オーバー時は`DropPolicy`に従って古いデータを破棄
- Transcribe送信成功後も一定期間保持（リトライに備える）

### TranscribeClient モジュール

**責務**: AWS Transcribe Streaming APIとの通信

**無音時の動作仕様**:
- VADで無音判定時も、最低3分間は無音FLACデータを送り続ける。
- 3分間完全無音が続いた場合はTranscribeストリームを一旦切断。
- その後、音声が再検出されたら自動でTranscribeストリームを再開する。
- 無音FLACデータはエンコーダで生成した無音チャンクを送信する。
- このロジックは`src/transcribe.rs`で実装。

**主要インターフェース**:
```rust
pub struct TranscribeClient {
    config: TranscribeConfig,
    retry_policy: RetryPolicy,
    buffer_ref: Arc<Mutex<AudioBuffer>>,
}

impl TranscribeClient {
    pub async fn new(config: TranscribeConfig) -> Result<Self>;
    pub async fn send_audio(&mut self, samples: &[i16]) -> Result<()>;
    pub async fn receive_transcripts(&mut self) -> Result<Option<TranscriptResult>>;
}
```

**リトライ戦略**:
- 指数バックオフ: 1秒 → 2秒 → 4秒 → 8秒（最大）
- 最大リトライ回数: 設定可能（デフォルト: 5回）
- リトライ時はバッファから該当区間のデータを再送
- 永続的な失敗の場合はエラーログ出力して継続

### WavWriter モジュール

**責務**: チャンネル毎のWAVファイル書き出し

**主要インターフェース**:
```rust
pub struct WavWriter {
    channel_id: usize,
    output_dir: PathBuf,
    current_file: Option<hound::WavWriter<BufWriter<File>>>,
    spec: hound::WavSpec,
}

impl WavWriter {
    pub fn new(channel_id: usize, output_dir: PathBuf, spec: hound::WavSpec) -> Self;
    pub fn write_samples(&mut self, samples: &[i16]) -> Result<()>;
    pub fn finalize(&mut self) -> Result<()>;
}
```

**ファイル管理**:
- チャンネル毎に独立したWAVファイル
- 無音区間を含む全データを記録
- ファイルサイズ/時間で分割可能（オプション）

### AudioOutput モジュール

**責務**: 選択されたチャンネルの音声を出力デバイスにリアルタイム送信

**主要インターフェース**:
```rust
pub struct AudioOutput {
    device: cpal::Device,
    sample_rate: u32,
    stream: Option<cpal::Stream>,
    audio_tx: Option<mpsc::Sender<Vec<i16>>>,
}

impl AudioOutput {
    pub fn new(device_name: Option<&str>, sample_rate: u32) -> Result<Self>;
    pub fn start(&mut self) -> Result<mpsc::Sender<Vec<i16>>>;
    pub fn stop(&mut self);
    pub fn list_devices() -> Result<()>;
}
```

**動作**:
- TUIでチャンネルが選択されると、該当ChannelProcessorにSenderが設定される
- 音声サンプルがリアルタイムでmpsc channelを通じて送信される
- cpalの出力ストリームでデバイスに再生
- バッファ不足時は無音で埋めて途切れを防止

### OutputFormatter モジュール

**責務**: タイムスタンプ付きJSON出力

**主要インターフェース**:
```rust
pub struct OutputFormatter {
    start_time: SystemTime,
}

impl OutputFormatter {
    pub fn new() -> Self;
    pub fn format(&self, channel_id: usize, result: TranscriptResult) -> String;
}
```

**出力仕様**:
- JSON Lines形式（1行1JSON）
- ISO 8601形式のタイムスタンプ
- Unixエポック秒も併記
- チャンネルID、部分結果フラグを含む

## エラーハンドリング戦略

### ネットワークエラー

**発生場所**: TranscribeClient

**対策**:
1. **接続タイムアウト**: 設定可能なタイムアウト（デフォルト: 10秒）
2. **送信失敗時**:
   - AudioBufferから該当区間を再取得
   - 指数バックオフでリトライ（1秒 → 2秒 → 4秒 → 8秒）
   - 最大リトライ回数に達したらエラーログ出力して継続
3. **接続断**:
   - 自動再接続（バックオフ付き）
   - 再接続中もバッファリング継続
   - バッファ溢れ時は最古データから破棄

### バッファオーバーフロー

**発生場所**: AudioBuffer

**対策**:
1. **DropPolicy.DropOldest**: 最古のチャンクから破棄
2. ログに警告を出力
3. 破棄されたデータの期間を記録（後でギャップとして報告可能）

### デバイスエラー

**発生場所**: AudioInput

**対策**:
1. **デバイス接続失敗**:
   - エラーログを出力して終了
   - または利用可能なデバイス一覧を表示
2. **ストリームエラー**:
   - エラーログ出力
   - 可能であれば再初期化を試みる

### WAVファイル書き込みエラー

**発生場所**: WavWriter

**対策**:
1. **ディスク容量不足**:
   - エラーログ出力
   - 該当チャンネルの録音を停止（Transcribeは継続）
2. **ファイルアクセスエラー**:
   - エラーログ出力
   - 代替パスで再試行

### Transcribe APIエラー

**発生場所**: TranscribeClient

**対策**:
1. **認証エラー**:
   - エラーログ出力して終了（設定の見直しが必要）
2. **レート制限**:
   - 指数バックオフでリトライ
   - リトライ中もバッファリング継続
3. **その他APIエラー**:
   - エラー内容をログ出力
   - リトライ可能ならリトライ、不可能なら該当チャンクをスキップ

## 設定ファイル仕様

設定ファイル (`config.toml`) の構造:

```toml
[audio]
device_id = "default"  # または具体的なデバイスID
sample_rate = 16000    # Hz (16kHz - AWS Transcribeの推奨値、量子化ビット数は16bit固定)
channels = 4           # チャンネル数

[vad]
threshold_db = -40.0           # dB
hangover_duration_ms = 500     # ms

[buffer]
capacity_seconds = 300         # 秒
drop_policy = "drop_oldest"    # drop_oldest | drop_newest | block

[transcribe]
region = "ap-northeast-1"      # 東京リージョン
language_code = "ja-JP"        # 日本語
sample_rate = 16000            # Transcribeに送信するサンプルレート (16kHz)
max_retries = 5
timeout_seconds = 10

[output]
wav_output_dir = "./recordings"
log_level = "info"             # error | warn | info | debug

[[channels]]
id = 0
name = "無線機1"
enabled = true

[[channels]]
id = 1
name = "無線機2"
enabled = true

# 以下、必要なチャンネル数分定義
```

## パフォーマンス考慮事項

### CPU使用率
- チャンネル毎に独立スレッド → CPU並列化が重要
- VADは軽量（RMS計算のみ）
- Transcribeへの送信は非同期I/O

### メモリ使用量
- バッファサイズ × チャンネル数が主要な消費源
- 30秒バッファ × 4チャンネル × 16kHz × 2byte ≈ 3.84 MB
- 非常に軽量

### ネットワーク帯域
- 音声データサイズ: 16kHz × 2byte = 32 KB/s per channel
- 4チャンネル × 32 KB/s = 128 KB/s ≈ 1 Mbps
- 5G回線では十分

## TUI (Terminal User Interface) モジュール

### 概要

ratatuiとcrosstermを使用したリアルタイム監視インターフェース。
各チャンネルの状態を1つのスクリーンに表示。

### 表示内容

各チャンネルごとに以下の情報を表示：

1. **ボリュームバー（2行）**
   - リアルタイム入力ボリューム（シアン、200msecごとに更新）
   - 瞬間最大ボリューム（イエロー、3秒単位）
   - 範囲: -60dB ~ 0dB

2. **ステータス行（1行）**
   - VAD状態: 灰色（無音） / 青色（音声あり）
   - Transcribe接続状態: 緑色（正常） / 赤色（エラー） / 灰色（無通信）

3. **文字起こし結果（残りスペース）**
   - 最新10件の文字起こしテキスト
   - HH:MM形式の時刻付き

### TuiState モジュール

**責務**: TUIとChannelProcessor間の状態共有

```rust
pub struct TuiState {
    channels: Arc<Mutex<Vec<ChannelState>>>,
}

pub struct ChannelState {
    pub channel_id: usize,
    pub channel_name: String,
    pub current_volume_db: f32,
    pub peak_volume_db: f32,
    pub vad_state: VadState,
    pub transcribe_status: TranscribeStatus,
    pub transcripts: VecDeque<TranscriptEntry>,
}

pub enum TranscribeStatus {
    Connected,   // 正常
    Error,       // エラー
    Disconnected, // 無通信
}
```

**更新タイミング**:
- ボリューム: 各音声チャンク処理時（ChannelProcessor::process_chunk）
- VAD状態: 各音声チャンク処理時
- Transcribe状態: 送信成功/失敗時
- 文字起こし結果: Transcribe結果受信時

**スレッドセーフ**:
- Arc<Mutex<>>で保護された共有状態
- 各ChannelProcessorとTUIタスクから並行アクセス可能

### 操作

- `q` または `Esc`: アプリケーション終了
- 自動更新: 200msecごと

## 今後の拡張性

- **複数の文字起こしバックエンド**: Google Speech-to-Text, Azure等への対応
- **話者識別**: チャンネル内での複数話者の識別
- **感情分析**: 音声の感情解析
- **WebUIダッシュボード**: ブラウザベースの可視化（TUIの代替）
- **クラウドストレージ連携**: WAVファイルの自動アップロード
