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

- **スマホ・PC 両対応** — スマホは上部のタブ、PC(幅 900px 以上)は左サイドバーにチャンネルのカード + 中央に読みやすい幅のタイムライン。PC は数字キーでタブ切替、`L` でライブ、`Esc` で再生停止
- **チャンネルタブ** — 「すべて / 各チャンネル」で表示を切り替え(localStorage に保存)。連続再生も表示中のチャンネルだけを対象にする
- **受信状態のリアルタイム表示** — 各チャンネルの受信中(赤い点・経過秒)/ 最終受信からの経過 / 音量メーター / Transcribe 接続(T)を表示。変化したときだけ数十バイトを送る
- **チャンネル色分け** — TUI と同様、行ごとに背景色で識別
- **新しい順** — 新着が一番上。行は発話・録音の時刻順に並ぶ(遅れて届いた結果も時刻の位置に入る)。過去分は下端で読み込む
- **転送量極小化** — 確定結果のみデフォルト送信、JSON フィールド 1 文字短縮 (`c/s/x/p`)、
  外部 CDN なしの単一 HTML を起動時に brotli 圧縮(約 30KB → 約 9.5KB)し ETag で 304、
  大きめの JSON も brotli、音声は HE-AAC の低ビットレート
- **多クライアント耐性** — `tokio::sync::broadcast` で各クライアント独立、互いに影響しない
- **スロークライアント切断** — 一定量以上遅れたクライアントは自動切断 → 他クライアントに影響なし
- **回線断耐性** — 各イベントに単調増加 ID を付与、再接続時 `Last-Event-ID` で欠損分のみ再送(重複なし)
- **既存パイプラインから完全隔離** — 同期非ブロッキングの `state.push()` のみ、Mutex は数 µs レベル
- **発話ごとの録音・再生** — 無音で区切った AAC を保存し、文字起こしの行から再生。文字起こしが出なかった通信も「(文字起こしなし)」行として再生可能
- **連続再生 → リアルタイム再生** — 録音を ▶ すると、聞き終わるたびに次に新しい録音へ進み、最新まで追いついたらライブ再生に切り替わる(ライブのチャンネルが未選択なら、表示中のタブのチャンネルを選ぶ)。ヘッダの「連続再生 ✕」で停止。タブで表示していないチャンネルは飛ばす
- **バックグラウンド再生** — 録音とライブで 1 つの audio 要素を使い回すので、画面ロック中・別アプリ表示中も連続再生 → ライブへ移れる。ロック画面・通知に再生中の内容と停止 / 次へのボタンが出る(Media Session)。ただしバックグラウンドの間は文字起こしの受信(SSE)が止まることがあり、戻ったときに最新分を取り直す
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
profile = "he"                 # "he"(HE-AAC、16〜32kbps 向け) か "lc"(AAC-LC、48kbps 以上向け)
bitrate = 24000                # AAC のビットレート(HE-AAC 24kbps で 10 秒 ≒ 30KB)
silence_split_ms = 2000        # この長さ無音が続いたら区切る
pre_roll_ms = 300              # 発話の直前も少し含める(頭切れ防止)
tail_ms = 300                  # 末尾に残す無音
min_voice_ms = 300             # 発話がこれ未満ならノイズとして捨てる
max_duration_secs = 300        # 長すぎる区間はここで分割
```

区間の判定には `[vad] threshold_db` を使う(VAD のハングオーバーとは独立)。
ファイルは `recordings/segments/<起動日時>/ch<id>_<HHMMSS_mmm>_<区間ID>.aac` に保存される
(HE-AAC/ADTS、モノラル、サンプルレートは入力と同じ。`format = "wav"` なら 16bit WAV)。
自動削除はしないので、不要になったら手で消すこと。
文字起こしの行には、その結果を受信した時点で録音中(または直前)だった区間がひも付く。

### 3. ライブ再生を有効にする(任意)

```toml
[live]
enabled = true
profile = "he"    # HE-AAC
bitrate = 16000   # 聞いている 1 ミックスあたりの帯域(16kbps ≒ 2KB/秒)
```

閲覧画面のヘッダに「ライブ」の行が出る。聞きたいチャンネルを選んで「▶ 聞く」。

- 複数チャンネルを選ぶと、サーバ側で足し合わせて 1 本の AAC ストリームにする(チャンネル間の時間ずれなし)
- 同じ組み合わせを聞いている人同士はエンコーダを共有する。誰も聞いていなければエンコードもしない
- 再生中にチャンネルを切り替えると、その場でつなぎ直す
- 録音(▶ n秒)を再生する間はライブを止め、終わると再開する
- **低遅延再生**: WebCodecs に対応したブラウザ(Chrome / Edge / 新しめの Safari など)では、`fetch` で受け取った AAC を
  `AudioDecoder` でデコードし Web Audio で再生する。ブラウザ内に溜める量は 0.15〜1 秒で回線に合わせて自動調整
  (途切れたら増やし、30 秒安定したら減らす。溜まりすぎたら古いフレームを捨てる)。遅延の目安:

  | 内訳 | 時間 |
  |---|---|
  | 入力バッファ(1024 フレーム) | 約 21ms |
  | AAC エンコード + デコード | HE-AAC 約 163ms / AAC-LC 約 79ms(`profile = "lc"` で 84ms 短くなるが帯域は約 2 倍) |
  | ブラウザ内のバッファ | 0.15〜1 秒(安定していれば 0.2〜0.3 秒) |
  | ネットワーク(Cloudflare 経由) | 回線次第 |

  画面が隠れている間はバックグラウンドで再生を続けやすい `<audio>` に切り替え、戻ったら低遅延に戻す。
  WebCodecs 非対応のブラウザは `<audio>` で再生する(約 2.3 秒遅れる)
- **iPhone (Safari)**:
  - 低遅延再生には WebCodecs の `AudioDecoder` が必要で、Safari では **iOS 26 以降**。それより前の iOS は `<audio>` で再生する(約 2.3 秒遅れる)
  - デコーダが ADTS を受け付けない・音が出てこない場合は、自動で `<audio>` に切り替える
  - iOS は画面ロック・バックグラウンドで Web Audio を止めるので、画面が隠れたら `<audio>` に切り替えて再生を続け、戻ったら低遅延に戻す。
    割り込み(タイマー音など)のあと AudioContext が戻らない既知の不具合があるため、2 秒以上止まっていたら次の操作まで `<audio>` を使う
  - バックグラウンド再生は Safari のタブで使うこと。ホーム画面に追加した Web アプリはロック画面の操作が効かない報告がある
- サーバは TCP_NODELAY を有効にして、小さなフレームを溜めずにすぐ送る
- `<audio>` で再生するときは、回線の揺れで遅延が溜まらないよう、ブラウザ側で溜まった分を監視する:
  3 秒を超えたら少し速く再生、5 秒を超えたら今の音まで飛ぶ、10 秒以上止まったらつなぎ直す。
  遅延が 3 秒を超えるとボタンに「遅延 n秒」と出る(Chrome は再生を続けるのに約 2.3 秒分を溜めるので、これが下限)
- 接続直後にサーバが無音の AAC を 32KB 送る(プリフィル)。Chrome は受信データを 32KB 単位で扱うため、
  これが無いと 16kbps では再生開始まで十数秒かかる。溜まった無音は再生開始時にブラウザが飛ばす。
  接続 1 回あたり 32KB 余分にかかる(`/live?...&prefill=0` で無効化できる)
- サーバ側でも、遅れた聴取者には古いフレームを飛ばして送る

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

本体に cloudflared の起動を任せる(推奨):

```toml
[tunnel]
enabled = true
config = "~/.cloudflared/dcr-transcribe.yml"   # 上で作った設定ファイル
# token_file = "..."                           # ダッシュボードで作ったトンネルならトークンを書いたファイル
```

本体の起動と一緒に cloudflared が立ち上がり、落ちたら 1 秒 → 最大 30 秒の間隔で起動し直し、本体の終了時に止まる
(SIGTERM、3 秒で終わらなければ強制終了)。cloudflared の出力は本体のログに入る(接続できると「トンネル: 接続しました」)。

手動で動かす場合は、本体を起動したあと別ターミナルで:

```bash
cloudflared tunnel --config ~/.cloudflared/dcr-transcribe.yml run
```

`https://transcribe.example.com` をブラウザで開くと、リアルタイムに文字起こしが流れます。
(ダッシュボードの Zero Trust → Networks → Tunnels で作った場合は、トークンをファイルに書いて `token_file` に指定する)

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

**C. 国内 IP に限定 + bot 確認(本体側)**

```toml
[remote]
allowed_countries = ["JP"]          # 日本からのアクセスだけ許可
turnstile_site_key = "0x4AAAA..."   # Cloudflare Turnstile
turnstile_secret_key = "0x4AAAA..."
session_hours = 12                  # 確認を通ったあと再確認なしで使える時間
```

- **国の制限**: Cloudflare がトンネル経由のリクエストに付ける `CF-IPCountry` で判定し、許可していない国は 403。
  Tor(`T1`)や判定不能(`XX`)も通さない。ヘッダが無いのはローカル(`localhost` / `127.0.0.1`)から直接来たときだけ許可する。
  Cloudflare ダッシュボードの Network → **IP Geolocation** が有効であること(既定で有効)。
  海外の VPN や iCloud プライベートリレー経由だと国外扱いになることがある
- **bot 確認**: 閲覧画面を開くと Turnstile の確認画面が出て、通過するとセッションクッキー(HttpOnly / Secure)が発行される。
  SSE・録音・ライブなどのデータ系はこのクッキーが無いと 403。期限が切れたら画面が自動で確認画面に戻る。
  Cloudflare のチャレンジ画面と違い、SSE や音声の読み込みが期限切れで黙って止まることはない。
  セッションはメモリに持つので、本体を再起動すると確認し直しになる
- Turnstile のキーは、ダッシュボードの **Turnstile → Add widget** でホスト名(`transcribe-private.bunfree.net` など)を登録して発行する。
  動作確認だけなら Cloudflare 公開のテスト用キー(サイトキー `1x00000000000000000000AA`、シークレット `1x0000000000000000000000000000000AA`、常に通る)が使える
- ゾーンで Cloudflare のチャレンジ(「Just a moment...」)が有効になっている場合は、このホスト名では外しておく(SSE や音声の読み込みが止まるため)

**D. Cloudflare Access**

Zero Trust ダッシュボードの Access → Applications で `transcribe.example.com` を Self-hosted アプリとして登録し、
メールアドレスのワンタイムコードなどで閲覧者を絞る。アプリ側の認証と違い、トンネルの手前で弾ける。

A〜D はすべて併用可能(全てを通過する必要あり、二段階以上のゲート構成)。
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
| HTML 初回 | 約 30KB → brotli で **約 9.5KB**。2 回目以降は ETag で 304(本文 0 バイト) |
| 1 イベント (SSE) | 約 80-200 bytes(発話の長さによる) |
| Keep-alive | 15 秒ごとにコメント行(数 bytes) |
| 1 時間あたり(100 発話) | 約 15KB + keep-alive ≒ **18KB/h** |
| 再接続(短時間切断) | 0-数百 bytes(`Last-Event-ID` で欠損分のみ送信) |
| 再接続(長時間切断・履歴脱落) | 直近 200 件分まで(40-50KB 程度) |
| 録音区間の通知 (`seg`) | 1 区間あたり約 50 bytes。再接続時は未取得分のみ `/segments?after=` で取得 |
| 受信状態 (`st`) | 変化したときだけ。受信中で音量が大きく揺れていても約 60 bytes/秒、無受信なら 0 |
| 録音一覧 (`/segments`) | 初回は新しい側 300 件まで、brotli 圧縮。以降は差分のみ |
| 録音の再生 | 再生したときだけ。HE-AAC 24kbps で約 3KB/秒(10 秒の通信で約 30KB) |
| ライブ再生 | 聞いている間ずっと。HE-AAC 16kbps で約 2KB/秒 ≒ **7MB/時**(無音中も同じ) |

### 細い・遅延の大きい回線(4G/5G モバイルルータ)について

経路は `ブラウザ ⇄ Cloudflare エッジ ⇄ cloudflared ⇄ 本体(127.0.0.1)` の 3 区間:

| 区間 | プロトコル | 調整できること |
|---|---|---|
| ブラウザ ⇄ Cloudflare エッジ | HTTP/2 (TCP) か HTTP/3 (QUIC) | **ここがボトルネック**。TCP のパラメータは端末 OS と Cloudflare 側で決まり、こちらからは変えられない。ダッシュボードの Speed → Optimization → Protocol Optimization で **HTTP/3 (with QUIC)** と **0-RTT** を有効にすると、パケットロスや遅延の揺れに強くなり再接続も速い |
| Cloudflare エッジ ⇄ cloudflared | QUIC(`protocol=quic`、既定) | 調整不要 |
| cloudflared ⇄ 本体 | ループバック TCP | 遅延ゼロなので TCP の調整は効果がない |

そのため、アプリ側では「流す量を減らす」「遅延が溜まったら捨てる」「切れたら差分だけ取り直す」で対処している:

- SSE は `Last-Event-ID` で差分のみ再送、長時間バックグラウンドの後は直近 20 件だけ取得
- 受信状態は変化したときだけ送る。取りこぼしたら現状をまとめて送り直す
- ライブ音声は HE-AAC 16kbps。遅れた聴取者にはサーバが古いフレームを飛ばし、ブラウザも溜まった分を早送り・スキップする
- 録音は再生するときだけ取得(Range 対応)

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
