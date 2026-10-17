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

## リモート閲覧 (HTTP/SSE + Cloudflare Tunnel)

ローカルマシン内に軽量 HTTP サーバを立て、Cloudflare Tunnel (`cloudflared`) 経由で外部から `tail -f` 風にライブ閲覧する機能。
発話ごとの録音(`[segments]`)を有効にすると、各行の ▶ ボタンでその通信の音声を再生できる。
ライブ再生(`[live]`)を有効にすると、チャンネルを選んでリアルタイムに聞ける(複数選ぶとサーバ側でミックス)。

### 特徴

- **モバイルファースト UI** — スマホ前提、レスポンシブ、safe-area 対応、システムフォントで日本語綺麗
- **チャンネルフィルタ** — チャンネル毎にチップで表示/非表示を切替(localStorage 永続化)
- **チャンネル色分け** — TUI と同様、行ごとに背景色で識別
- **新しい順** — 新着が一番上。行は発話・録音の時刻順に並ぶ(遅れて届いた結果も時刻の位置に入る)。過去分は下端で読み込む
- **転送量極小化** — 確定結果のみデフォルト送信、JSON フィールド 1 文字短縮 (`c/s/x/p`)、
  外部 CDN なしの単一 HTML(gzip 後 ~3KB)
- **多クライアント耐性** — `tokio::sync::broadcast` で各クライアント独立、互いに影響しない
- **スロークライアント切断** — 一定量以上遅れたクライアントは自動切断 → 他クライアントに影響なし
- **回線断耐性** — 各イベントに単調増加 ID を付与、再接続時 `Last-Event-ID` で欠損分のみ再送(重複なし)
- **既存パイプラインから完全隔離** — 同期非ブロッキングの `state.push()` のみ、Mutex は数 µs レベル
- **発話ごとの録音・再生** — 無音で区切った AAC を保存し、文字起こしの行から再生。文字起こしが出なかった通信も「(文字起こしなし)」行として再生可能
- **連続再生** — 録音を ▶ すると、聞き終わるたびに次に新しい録音へ進む。最新まで追いついたら新着を待って続けて再生(ヘッダの「連続再生 ✕ / 新着待ち ✕」で停止)。非表示にしたチャンネルは飛ばす。ライブを聞いていた場合は、最新まで追いついたらライブに戻る
- **ライブ再生** — ヘッダの「ライブ」でチャンネルを選んで ▶。複数チャンネルはサーバでミックスして 1 本の AAC ストリームにする
- **トンネル障害でもメイン処理は無傷** — `cloudflared` と本体は別プロセスで完全に独立

### 1. config.toml に `[remote]` セクションを追加

```toml
[remote]
enabled = true
listen = "127.0.0.1:8765"   # loopback で listen、cloudflared が公開
history_size = 200          # 新規クライアントへ送信する履歴件数
include_partials = false    # partial も配信するか(転送量増)
partial_throttle_ms = 500   # partial の throttle(include_partials=true 時のみ有効)
auth_token = ""             # 共有トークン。設定すると `?t=...` 必須
```

### 2. 発話ごとの録音を有効にする(任意)

```toml
[segments]
enabled = true
dir = "./recordings/segments"  # 起動ごとに日時名のサブディレクトリを作る
format = "aac"                 # "aac"(ADTS、小さく配信向き) か "wav"
bitrate = 64000                # AAC のビットレート
silence_split_ms = 2000        # この長さ無音が続いたら区切る
pre_roll_ms = 300              # 発話の直前も少し含める(頭切れ防止)
tail_ms = 300                  # 末尾に残す無音
min_voice_ms = 300             # 発話がこれ未満ならノイズとして捨てる
max_duration_secs = 300        # 長すぎる区間はここで分割
```

区間の判定には `[vad] threshold_db` を使う(VAD のハングオーバーとは独立)。
ファイルは `recordings/segments/<起動日時>/ch<id>_<HHMMSS_mmm>_<区間ID>.aac` に保存される
(AAC-LC/ADTS、モノラル、サンプルレートは入力と同じ。`format = "wav"` なら 16bit WAV)。
自動削除はしないので、不要になったら手で消すこと。
文字起こしの行には、その結果を受信した時点で録音中(または直前)だった区間がひも付く。

### 3. ライブ再生を有効にする(任意)

```toml
[live]
enabled = true
bitrate = 48000   # 聞いている 1 ミックスあたりの帯域(48kbps ≒ 6KB/秒)
```

閲覧画面のヘッダに「ライブ」の行が出る。聞きたいチャンネルを選んで「▶ 聞く」。

- 複数チャンネルを選ぶと、サーバ側で足し合わせて 1 本の AAC ストリームにする(チャンネル間の時間ずれなし)
- 同じ組み合わせを聞いている人同士はエンコーダを共有する。誰も聞いていなければエンコードもしない
- 再生中にチャンネルを切り替えると、その場でつなぎ直す
- 録音(▶ n秒)を再生する間はライブを止め、終わると再開する
- 遅延はブラウザのバッファ次第で数秒程度。回線が詰まったら古い音を飛ばして追いつく

AAC エンコードには Fraunhofer FDK AAC(`fdk-aac` クレート、ソース同梱でビルド)を使う。
FDK AAC は独自ライセンスなので、バイナリを配布する場合はライセンス条件を確認すること。

### 4. 起動

```bash
cargo run --release
```

ログに `Remote サーバ起動: http://127.0.0.1:8765 (auth=off, partials=false)` が出ればOK。

### 5. Cloudflare Tunnel で公開

> **Quick Tunnel (`cloudflared tunnel --url ...` / trycloudflare.com) は SSE 非対応なので使えない。**
> Cloudflare に登録済みのドメインで名前付きトンネルを作る。

初回のみ:

```bash
brew install cloudflared
cloudflared tunnel login                                  # ブラウザでドメインを選んで認可
cloudflared tunnel create dcr-transcribe                  # ~/.cloudflared/<UUID>.json が作られる
cloudflared tunnel route dns dcr-transcribe transcribe.example.com
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: dcr-transcribe
credentials-file: /Users/<you>/.cloudflared/<UUID>.json
ingress:
  - hostname: transcribe.example.com
    service: http://127.0.0.1:8765
  - service: http_status:404
```

本体を起動したあと、別ターミナルで:

```bash
cloudflared tunnel run dcr-transcribe
```

`https://transcribe.example.com` をブラウザで開くと、リアルタイムに文字起こしが流れます。
(ダッシュボードの Zero Trust → Networks → Tunnels で作った場合は `cloudflared tunnel run --token <TOKEN>` でも可)

録音ファイルは `Cache-Control: private`、ライブは `no-store` で返すので、Cloudflare のエッジにはキャッシュされない。

### セキュリティ

外部公開する場合、以下のいずれかで認証してください:

**A. アプリ側で HTTP Basic 認証** (推奨)
```toml
[remote]
basic_auth_user = "viewer"
basic_auth_pass = "ランダムな長いパスワード"
```

ブラウザでアクセスすると認証ダイアログが出ます。一度入力すればブラウザがキャッシュし、
SSE 接続(EventSource)も自動的に同じ認証情報を送信します。

**B. アプリ側でトークン認証**
```toml
[remote]
auth_token = "ランダムな長い文字列"
```

クライアントは `https://transcribe.example.com/?t=<token>` でアクセス。
ブックマーク等で URL に埋め込めるので「リンク 1 つで開ける」用途に向く。

**C. Cloudflare Access**

Zero Trust ダッシュボードの Access → Applications で `transcribe.example.com` を Self-hosted アプリとして登録し、
メールアドレスのワンタイムコードなどで閲覧者を絞る。アプリ側の認証と違い、トンネルの手前で弾ける。

A〜C はすべて併用可能(全てを通過する必要あり、二段階以上のゲート構成)。
Basic 認証・トークンは `/`, `/stream`, `/history`, `/segments`, `/audio/{id}`, `/live` に適用(トークンは `/` 以外)、`/healthz` は常に認証不要(外部監視用)。

### URL クエリパラメータ

| パラメータ | 説明 |
|---|---|
| `?t=<token>` | `auth_token` 設定時に必須 |
| `?p=1` | partial 結果も表示(`include_partials=true` で送信されている場合のみ) |
| `?t=X&p=1` | 両方併用 |

### 転送量の目安

確定結果のみ(デフォルト)で:

| 項目 | サイズ |
|---|---|
| HTML 初回 | 約 6.7KB(gzip 後 ~2.9KB) |
| 1 イベント (SSE) | 約 80-200 bytes(発話の長さによる) |
| Keep-alive | 15 秒ごとにコメント行(数 bytes) |
| 1 時間あたり(100 発話) | 約 15KB + keep-alive ≒ **18KB/h** |
| 再接続(短時間切断) | 0-数百 bytes(`Last-Event-ID` で欠損分のみ送信) |
| 再接続(長時間切断・履歴脱落) | 直近 200 件分まで(40-50KB 程度) |
| 録音区間の通知 (`seg`) | 1 区間あたり約 50 bytes。再接続時は未取得分のみ `/segments?after=` で取得 |
| 録音の再生 | 再生したときだけ。AAC 64kbps で約 8KB/秒(10 秒の通信で約 80KB) |
| ライブ再生 | 聞いている間ずっと。48kbps で約 6KB/秒 ≒ **22MB/時**(無音中も同じ) |

### 動作確認

ローカルで動作確認:
```bash
curl http://127.0.0.1:8765/healthz       # → "ok"
curl http://127.0.0.1:8765/              # → HTML
curl -N http://127.0.0.1:8765/stream     # → SSE 接続(Ctrl+C で切断)

# 欠損補完の動作確認:
# 一度 stream に繋いで `id: 5` まで受信したと仮定 → 再接続時に Last-Event-ID を渡すと
# それ以降のみ受信できる
curl -N -H "Last-Event-ID: 5" http://127.0.0.1:8765/stream

# 録音区間
curl http://127.0.0.1:8765/segments              # → {"items":[{"i":1,"c":2,"t":...,"d":...}]}
curl -o seg.aac http://127.0.0.1:8765/audio/1

# ライブ(ch2 と ch3 のミックス)を 10 秒録って確認
curl -s --max-time 10 -o live.aac "http://127.0.0.1:8765/live?c=2,3"; ffprobe live.aac
```

### トラブルシュート

| 症状 | 対処 |
|------|------|
| ブラウザで「切断」のまま | サーバが起動しているか確認: `curl http://127.0.0.1:8765/healthz` |
| 公開 URL で "401 Unauthorized" | Basic 認証設定済みなら正しい user:pass を入力(またはブラウザのキャッシュをクリア)。token 設定済みならクエリ `?t=<token>` を付ける |
| 履歴が表示されない | F12 開発者ツールで `/stream` の SSE 受信状況を確認 |
| 表示にラグがある | トンネル経由は中継分のレイテンシがある。LAN 直結なら遅延ほぼゼロ |
| 公開 URL で「接続中」にならない | Quick Tunnel を使っていないか確認(SSE 非対応)。名前付きトンネルが必要 |
| ▶ が「録音中」のまま | 区間がまだ閉じていない(`silence_split_ms` の無音待ち)。閉じると自動で再生可能になる |
| ライブが「接続中…」のまま | 選んだチャンネルが有効か確認(`curl "http://127.0.0.1:8765/live?c=2"` が 400 なら無効なチャンネル)。音声入力が止まっていないか TUI で確認 |
| ▶ が「録音なし」 | サーバの保持件数(1000 区間)を超えて一覧から消えた。ファイルは `recordings/segments/` に残っている |
| 「再接続中…」のまま戻らない | サーバが再起動した可能性。`anchor` 変更時はブラウザリロードを推奨 |
| ログに「スロークライアントを切断」 | クライアント側ネットが詰まっている。自動再接続で復帰、欠損は `Last-Event-ID` で補完される |

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
