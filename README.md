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
  - 入力ボリューム（リアルタイム）
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

アプリの設定ファイルに認証情報を指定できます。GUI・TUI とも同じ設定を使います。
リージョンは `[transcribe] region = "ap-northeast-1"` です。

```toml
[transcribe.aws]
access_key_id = "YOUR_ACCESS_KEY_ID"
secret_access_key = "YOUR_SECRET_ACCESS_KEY"
# session_token = "..." # 一時認証情報を使う場合
```

既存の AWS プロファイルを使う場合は、アクセスキーの代わりに
`[transcribe.aws] profile = "default"` を指定します。
プロファイルとアクセスキーの同時指定や、片方だけのキーはエラーになります。
`aws login` のプロファイルは期限が切れたら再認証が必要です。
認証情報はデバッグ表示では伏せます。キーを書いた設定ファイルは権限 `600` にして Git に入れないでください。

未指定なら従来どおり AWS SDK の標準認証を使います。環境変数を使う例:

```bash
export AWS_ACCESS_KEY_ID="your-access-key"
export AWS_SECRET_ACCESS_KEY="your-secret-key"
export AWS_REGION="ap-northeast-1"
```

### 3. オーディオインターフェースの確認

この作業環境では `config.macbook.toml`（内蔵マイク1ch）と
`config.zoom-h6essential.toml`（H6essential の入力 Ch1〜Ch4）を用意しています。
両方とも中継サーバ経由で配信し、録音先は別フォルダです。
これらは認証情報を書き込むため Git 対象外にしています。
編集するのはこの2ファイルだけです。`config.toml` は MacBook 用へのシンボリックリンクで、
独立した設定ではありません。旧設定とエディタのバックアップは
`secrets/config-archive-20261106/` に退避しています。
`config.toml.example` は配布用の記入例で、実行時には使用しません。

```bash
cargo run --release -- --show-interfaces
```

利用可能な入力デバイスと出力デバイスの一覧が表示されます。

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
   - 現在の入力ボリューム（200msecごとに更新）
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
- `u`: 閲覧画面のログイン URL と QR コードを表示(もう一度 `u` か `Esc` で閉じる)
  - スマホのカメラで QR を読むと、そのままログインして閲覧画面が開く
  - ログインキーを含むので、開いたときだけ表示する(周りの人に見られないように)
  - 公開 URL は `[remote] public_url` で指定する。無ければ `[tunnel] config` の設定ファイルの `hostname` から組み立てる
  - `l`: 公開 URL とローカル URL(このパソコンからのみ)を切り替え
  - QR は白地に黒で描くので、端末の配色に関係なく読める。端末が小さいときは広げるよう案内が出る
- 画面の最下行にキー操作の案内が出る
- `1`～`0`: 対応するチャンネルの音声を出力デバイスに送る（トグル）
  - 選択されたチャンネルは黄色の枠で表示され、タイトルに `[出力中]` が表示されます
  - 同じ数字キーを再度押すと選択解除されます
  - 複数チャンネルを同時に選択でき、同時刻の入力をミックスして出力します
- TUIは自動的に200msecごとに更新されます

## デバイス警告と長時間録音

- 入出力デバイスのエラー、または音声コールバックが3秒以上途絶えた場合、TUIを赤い全画面と警告帯で1秒ごとに切り替える。無音の音声データが届いている状態では警告しない。
- ドライバエラーは最低3秒表示し、データが再び届けば解除する。デバイスを開き直す自動再接続は行わないため、挿し直しても警告が消えない場合はアプリを再起動する。
- 全音声WAVは音声の長さで1時間ごとに分割する。高いサンプルレートではWAVのサイズ上限に達する前に分割する。ファイル名の末尾に連番が付き、既存ファイルを上書きしない。
- 全音声WAVのヘッダーを約10秒ごとに更新する。プロセスが異常終了した場合も、最後の更新までの音声を読めるようにする。電源断時のディスクへの永続化を保証するものではない。
- 録音の書き込みに失敗した場合は対象の録音を停止し、TUIに警告を表示する。文字起こし処理は継続する。録音の再開には保存先を直してから再起動が必要。
- 48kHz・16bit・モノラルでは12時間あたり約4.15GB/チャンネルのWAVを保存する。発話区間の録音とログは別途必要。録音ファイルとログに自動削除・保存容量の上限はない。
- Web画面の文字起こし履歴は `remote.history_size` 件、発話録音の索引は直近1000件まで。ディスクに残っている全録音をWeb画面から検索できる仕組みではない。

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
- **リンクの共有** — ヘッダの「共有」で、ログイン URL を QR コード・スマホの共有シート(LINE / メッセージ / AirDrop など)・コピーで渡せる。
  リンクを知っている人は閲覧できるので、渡す相手に注意する(ログイン済みの人だけが取得できる)
- **受信状態のリアルタイム表示** — 各チャンネルの受信中(赤い点・経過秒)/ 最終受信からの経過 / 音量メーター / Transcribe 接続(T)を表示。変化したときだけ数十バイトを送る
- **チャンネル色分け** — TUI と同様、行ごとに背景色で識別
- **新しい順** — 新着が一番上。行は発話・録音の時刻順に並ぶ(遅れて届いた結果も時刻の位置に入る)。過去分は下端で読み込む
- **転送量極小化** — 確定結果のみデフォルト送信、JSON フィールド 1 文字短縮 (`c/s/x/p`)、
  外部 CDN なしの単一 HTML を起動時に brotli 圧縮し ETag で 304、
  大きめの JSON も brotli、音声は HE-AAC の低ビットレート
- **多クライアント耐性** — `tokio::sync::broadcast` で各クライアント独立、互いに影響しない
- **スロークライアント切断** — 一定量以上遅れたクライアントは自動切断 → 他クライアントに影響なし
- **回線断耐性** — 各イベントに単調増加 ID を付与、再接続時 `Last-Event-ID` で欠損分のみ再送(重複なし)
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
login_key_file = "./secrets/login_key"   # ログインキー(/#k=...)。無ければ初回起動時に生成
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
  遅延が 3 秒を超えるとボタンに「■ 停止 n秒」と出る(低遅延再生では常に表示)(Chrome は再生を続けるのに約 2.3 秒分を溜めるので、これが下限)
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

#### いちばん簡単: トークンと転送先を 1 ファイルにまとめる

ダッシュボードでトンネルを作ってトークンをコピーしたら(下の 1〜2)、次の 1 ファイルを作る。
Cloudflare 側に転送先(Public Hostname)が無ければ cloudflared はこのファイルの `ingress` を使うので、ダッシュボードでの転送先の設定もいらない。

`secrets/cloudflared.yml`(パーミッション 600 推奨。`secrets/` は git 管理外):

```yaml
token: eyJ...
ingress:
  - hostname: transcribe.example.com
    service: http://127.0.0.1:8765
  - service: http_status:404
```

```toml
[tunnel]
enabled = true
config = "./secrets/cloudflared.yml"
```

DNS がまだ無い・別のトンネルを指しているときは、一度だけ
`cloudflared tunnel route dns --overwrite-dns <トンネル名> transcribe.example.com` を実行する(要 `cloudflared tunnel login`)か、
ダッシュボードの DNS で `<トンネル ID>.cfargotunnel.com` への CNAME(Proxied)を作る。
別の PC では `config.toml` と `secrets/` をコピーするだけで動く(cloudflared も自動でダウンロードされる)。

#### ダッシュボードで作ってトークンを貼るだけ

1. Cloudflare ダッシュボード → **Zero Trust → Networks → Tunnels → Create a tunnel** → 「Cloudflared」を選び、名前(例: `dcr-transcribe`)を付ける
2. 「Install and run a connector」に表示されるコマンドの `eyJ...` で始まる長い文字列がトークン。これをコピーする(インストール作業は不要)
3. **Public Hostname** を追加: Subdomain `transcribe` / Domain `example.com` / Service `HTTP` `127.0.0.1:8765`
4. `config.toml` に貼る:

```toml
[tunnel]
enabled = true
token = "eyJ..."      # ダッシュボードのトークン(ファイルに置くなら token_file = "...")
```

これだけで、本体の起動と一緒にトンネルがつながる。`cloudflared tunnel login`・cert.pem・yml・`route dns` はいらない。
別の PC に移すときも `config.toml` を持っていくだけ(トークンは秘密なので扱いに注意。`config.toml` は git 管理外)。

- **cloudflared のインストールも不要**: PATH にも `bin_dir`(既定 `./bin`)にも無ければ、Cloudflare 公式の GitHub リリース(最新版)から
  自動でダウンロードする(約 20MB、初回のみ)。macOS では Cloudflare の開発者署名が付いていることを確かめてから使う。
  ダウンロードさせたくなければ `auto_download = false`
- 本体の起動と一緒に cloudflared が立ち上がり、落ちたら 1 秒 → 最大 30 秒の間隔で起動し直し、本体の終了時に止まる
  (SIGTERM、3 秒で終わらなければ強制終了)。cloudflared の出力は本体のログに入る(接続できると「トンネル: 接続しました」)
- トークンは引数ではなく環境変数 `TUNNEL_TOKEN` で cloudflared に渡す(`ps` で見えない)

#### CLI で作る場合(ローカル管理のトンネル)

```bash
cloudflared tunnel login                                  # ブラウザでドメインを選んで認可
cloudflared tunnel create dcr-transcribe                  # ~/.cloudflared/<UUID>.json が作られる
cloudflared tunnel route dns dcr-transcribe transcribe.example.com
```

`~/.cloudflared/dcr-transcribe.yml`:

```yaml
tunnel: dcr-transcribe
credentials-file: /Users/<you>/.cloudflared/<UUID>.json
ingress:
  - hostname: transcribe.example.com
    service: http://127.0.0.1:8765
  - service: http_status:404
```

```toml
[tunnel]
enabled = true
config = "~/.cloudflared/dcr-transcribe.yml"
```

CLI で作ったトンネルは、ダッシュボードの Tunnels 一覧から **Migrate** するとダッシュボード管理に移せる。
移したあとは `cloudflared tunnel token dcr-transcribe` で出るトークンを `token` に貼れば、yml は不要になる。

`https://transcribe.example.com/#k=<ログインキー>` をブラウザで開くと、リアルタイムに文字起こしが流れます。

録音ファイルは `Cache-Control: private`、ライブは `no-store` で返すので、Cloudflare のエッジにはキャッシュされない。

### セキュリティ

外部公開する場合は、以下を組み合わせて守る:

**A. ログイン URL(本体側、推奨)**

```toml
[remote]
login_key_file = "./secrets/login_key"   # 既定値。無ければ初回起動時に生成して保存(パーミッション 600)
# login_key = "..."                      # 直接書くこともできる(こちらが優先)
```

- `https://transcribe.example.com/#k=<ログインキー>` を開くと、画面の script がキーをアドレスバーと履歴から消してから
  同じオリジンの `POST /login` で渡し、セッションクッキーをもらう。**`#` 以降はサーバに送られない**ので、キーが Cloudflare・
  アクセスログ・LINE などのリンクプレビューのサーバに一切渡らない。旧形式の `/?k=` も受け付ける(先読み・プレビューではセッションを作らない)
- 状態ボタンの内訳の「この端末からログアウト」で、セッションを消し、`Clear-Site-Data: "cache", "storage"` で端末のキャッシュ・保存データも消す
  (`"cookies"` は同じドメインのほかのサブドメインのクッキーまで消すので使わず、自分のクッキーだけ消す)
- クッキーは HTTPS では `__Host-dcr_session`(Secure・Path=/・Domain なし。サブドメインや平文の HTTP から差し込めない)、
  LAN の平文 HTTP では `dcr_session`(Secure なし)。どちらも HttpOnly・**SameSite=Strict**。
  Strict のクッキーは別サイト(LINE など)から開いた直後の要求には付かないので、その場合は同じオリジンから開き直す中継ページを挟む
- 違うキーは 0.5 秒待ってから 403(総当たりを遅くする。Cloudflare の回数制限と併用)
- キーは再起動しても変わらない(ファイルに保存)。URL は `dcr-transcribe --show-login-url` で表示する(キーはログに出さない)
- キーを変えたいときは `secrets/login_key` を消して起動し直す(新しいキーが生成される。以前のクッキーも再起動で無効になる)
- キー無しで開くと「ログイン用の URL を開いてください」(401)、違うキーは 403
- 旧版の `basic_auth_user` / `basic_auth_pass` / `auth_token` は廃止。設定に残っていると起動時にエラーになる
  (気付かずに認証無しで公開されるのを防ぐため)

**B. 国内 IP に限定 + bot 確認(本体側)**

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
  海外の VPN 経由だと国外扱いになることがある
- **Tor**: `block_tor = true`(既定)で、`allowed_countries` が空でも Tor(`T1`)を拒否する
- **iCloud プライベートリレー**: `block_private_relay = true` で拒否する。Cloudflare の無料プランでは判定できない
  (専用の項目が無く、作れる IP リストも 1 万件までで、Apple の出口 IP の一覧 約 28 万件が入らない)ので、
  本体が Apple の一覧(`private_relay_url`)を 1 日ごとに取ってきて `CF-Connecting-IP` と照らし合わせる。
  止めた人には「Safari の『ぁあ』→『IP アドレスを表示』で開き直す」案内を出す。一覧は `private_relay_cache` に写しを置き、
  取り直せないときは写しで続ける(一度も読めていないうちは止めない)
- **bot 確認**: 閲覧画面を開くと Turnstile の確認画面が出て、通過するとセッションクッキー(HttpOnly / Secure)が発行される。
  ログイン URL と併用すると、キーが正しい人だけに確認画面が出る(キーを知らない人には Turnstile も見せない)。
  SSE・録音・ライブなどのデータ系はこのクッキーが無いと 403。期限が切れたら画面が自動で確認画面に戻る。
  Cloudflare のチャレンジ画面と違い、SSE や音声の読み込みが期限切れで黙って止まることはない。
  セッションはメモリに持つので、本体を再起動すると確認し直しになる
- Turnstile のキーは、ダッシュボードの **Turnstile → Add widget** でホスト名(`transcribe-private.bunfree.net` など)を登録して発行する。
  動作確認だけなら Cloudflare 公開のテスト用キー(サイトキー `1x00000000000000000000AA`、シークレット `1x0000000000000000000000000000000AA`、常に通る)が使える
- ゾーンで Cloudflare のチャレンジ(「Just a moment...」)が有効になっている場合は、このホスト名では外しておく(SSE や音声の読み込みが止まるため)

**C. Cloudflare Access**

Zero Trust ダッシュボードの Access → Applications で `transcribe.example.com` を Self-hosted アプリとして登録し、
メールアドレスのワンタイムコードなどで閲覧者を絞る。アプリ側の認証と違い、トンネルの手前で弾ける。

**D. HTTP の守り(常に有効、`src/security.rs`)**

| 項目 | 内容 |
|---|---|
| CSP | 既定はすべて拒否(`default-src 'none'`)。インラインの script / style は起動時に計算した sha256 だけ許可し、`unsafe-inline` / `unsafe-eval` は使わない。閲覧画面は外部のものを一切読まない(`img/media/connect-src 'self'`)。`frame-ancestors 'none'`・`base-uri 'none'`・`form-action 'none'`・`object-src 'none'` |
| Trusted Types | 閲覧画面は `require-trusted-types-for 'script'; trusted-types 'none'`。`innerHTML` などへの文字列の代入をブラウザが拒否する(画面は DOM の組み立てと textContent だけで作っている) |
| CSP 違反の報告 | `report-uri` / `report-to` で `/csp-report` へ。ログの target `csp` に要点を 1 行で(1 分 20 件まで) |
| Permissions-Policy | 使う autoplay・clipboard-write・web-share(自分のオリジンだけ)以外は全部禁止(カメラ・マイク・位置情報・決済・USB など) |
| クロスオリジン | COOP `same-origin`・CORP `same-origin`・閲覧画面は COEP `require-corp`・`Origin-Agent-Cluster` |
| そのほか | `X-Content-Type-Options: nosniff`・`X-Frame-Options: DENY`・`Referrer-Policy: no-referrer`・`X-DNS-Prefetch-Control: off`・HTTPS のときは HSTS(2 年) |
| Fetch Metadata | Sec-Fetch-* を送るブラウザが前提(送らなければ「対応していません」)。エンドポイントごとに許す読み方を決める(下の表)。別サイトから来た要求はトップレベルのページ遷移だけ |
| 報告 | Reporting API で CSP 違反・**NEL(閲覧者のネットワークエラー)**・COOP / COEP 違反・非推奨・介入・クラッシュを `/reports` に受け、ログの target `report` に種類ごとの要点を出す。NEL は HTTPS のときだけ |
| キャッシュ | 全応答に `CDN-Cache-Control` / `Cloudflare-CDN-Cache-Control: no-store`(Cloudflare に残さない)。録音はブラウザには残してよいが `private, no-cache` + ETag で毎回確かめる(変わっていなければ 304) |
| 検索 | 全応答に `X-Robots-Tag: noindex, nofollow, noarchive, nosnippet, noimageindex` |
| ブラウザの版 | 開いたとき、UA Client Hints か User-Agent で版を見て、下限(既定 Chromium 系 120・Safari 16.4・Firefox 128。`[remote.browser]`)より古ければ断る。iPhone / iPad はどのブラウザも Safari の版で見る |
| メソッド | GET / HEAD と、`/verify`・`/csp-report` の POST だけ。ほかは 405 |
| `/verify` | Origin が自分と違えば 403 |
| 共有の QR | 自分のオリジンの URL だけ作る(よそへ誘導する QR に使わせない) |

Sec-Fetch の規則(`security::check_fetch`):

| パス | 許す読み方 |
|---|---|
| `/` | ページ遷移(`navigate` + `document`)。別サイトからのトップレベルの遷移も |
| `/audio/{id}` | 同じオリジンの `<audio>`(`audio` / `video`)。iOS ではメディアの読み込みが別経路になることがあるので、ヘッダが無いものも通す(セッションは必須。`/live` も同じ)。実機で付いているかはアクセスログの `site` / `mode` / `dest` で確かめられる |
| `/live` | 同じオリジンの `<audio>` と fetch(`audio` / `video` / `empty`) |
| `/share/qr.svg` | 同じオリジンの `<img>` |
| `/reports` `/csp-report` | ブラウザの報告(`report` / `empty`) |
| ほか | 同じオリジンの fetch / EventSource(`empty`、`navigate` 以外) |

curl で調べるときは `[remote] require_fetch_metadata = false` にする(本番では true のまま)。

HSTS は `includeSubDomains` を付けない(同じドメインのほかのサービスに影響するため)。付けるなら Cloudflare の HSTS 設定で行う。

**対応していないブラウザ**: EventSource・fetch・CSS grid などが無いブラウザや、JavaScript が無効・CSP で script が動かないブラウザには
「このブラウザには対応していません」と出す(案内を先に出しておき、対応を確かめた script が隠す)。

A〜D はすべて併用可能(全てを通過する必要あり、二段階以上のゲート構成)。
データ系(`/stream`, `/history`, `/segments`, `/audio/{id}`, `/live`)は有効なセッションが必須、`/healthz` は常に認証不要(外部監視用)。

### URL クエリパラメータ

| パラメータ | 説明 |
|---|---|
| `#k=<ログインキー>` | ログイン(画面の script が `POST /login` でキーを渡す。旧形式の `?k=` も可) |
| `?p=1` | partial 結果も表示(`include_partials=true` で送信されている場合のみ) |

### 転送量の目安

確定結果のみ(デフォルト)で:

| 項目 | サイズ |
|---|---|
| HTML 初回 | brotli 圧縮。2 回目以降は ETag で 304(本文 0 バイト) |
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

認証不要のヘルスチェック:

```bash
curl http://127.0.0.1:8765/healthz       # → "ok"
```

閲覧・再生はログイン URL をブラウザで開いて確認する。

### トラブルシュート

| 症状 | 対処 |
|------|------|
| ブラウザで「切断」のまま | サーバが起動しているか確認: `curl http://127.0.0.1:8765/healthz` |
| 「ログイン用の URL を開いてください」 | `dcr-transcribe --show-login-url` で表示した URL を開く。本体を再起動するとクッキーが無効になるので開き直す |
| 履歴が表示されない | F12 開発者ツールで `/stream` の SSE 受信状況を確認 |
| 表示にラグがある | トンネル経由は中継分のレイテンシがある。LAN 直結なら遅延ほぼゼロ |
| 公開 URL で「接続中」にならない | Quick Tunnel を使っていないか確認(SSE 非対応)。名前付きトンネルが必要 |
| ▶ が「録音中」のまま | 区間がまだ閉じていない(`silence_split_ms` の無音待ち)。閉じると自動で再生可能になる |
| ライブが「接続中…」のまま | 選んだチャンネルが有効か確認(`curl "http://127.0.0.1:8765/live?c=2"` が 400 なら無効なチャンネル)。音声入力が止まっていないか TUI で確認 |
| ▶ が「録音なし」 | サーバの保持件数(1000 区間)を超えて一覧から消えた。ファイルは `recordings/segments/` に残っている |
| 「再接続中…」のまま戻らない | サーバが再起動した可能性。`anchor` 変更時はブラウザリロードを推奨 |
| ログに「スロークライアントを切断」 | クライアント側ネットが詰まっている。自動再接続で復帰、欠損は `Last-Event-ID` で補完される |

## 中継サーバ方式(会場の回線が細いとき)

上の方式では、閲覧者ひとりひとりの通信(ライブ音声・録音の再生・再接続時の取り直し)がすべて会場の上り回線を通る。
閲覧者が数十人いると、Transcribe へ送る音声と取り合いになり、文字起こしそのものが遅れる。
中継サーバ方式では、東京などの VPS に中継サーバを置いて、会場からは **QUIC(UDP)1 本** で送るだけにする。
会場 ⇄ 中継は cloudflared を通さず、私設 CA の証明書で互いを確かめる(mTLS)。閲覧者は今までどおり cloudflared 経由の HTTPS。

```
[会場 Mac] ══ QUIC 443/udp(mTLS)══> [VPS: dcr-transcribe --relay] ── cloudflared(HTTPS)──> 閲覧者 N 人
  ├─ Transcribe へ(今までどおり)
  └─ ローカルの閲覧サーバ([remote])も併用できる
```

- **会場の上りは閲覧者の人数によらず一定** — 文字起こし・状態・区間録音(各 1 回)と、聞かれているチャンネルのライブ音声だけ
- **切れても続きから** — 文字起こしと区間録音には会場側で連番を振り、つなぎ直すと中継サーバが受け取り済みの位置を返すので、その続きだけ送る(重複・欠損なし)。終了時も未送信分を送り切ってから(最大 5 秒)閉じる
- **会場が切れても閲覧画面は生きている** — 履歴・録音はそのまま見られる。ヘッダに「会場と切断中 hh:mm:ss〜」と出て、チャンネルの状態は薄く表示される
- **会場を再起動しても履歴が続く** — 中継サーバの起動時刻を基準に時刻を付け替え、区間 ID も起動ごとに重ならないように振り直す
- **パケットが落ちても全部は止まらない** — 文字起こし・状態は優先度の高いストリーム、区間録音は 1 件ずつ優先度の低いストリーム、
  ライブ音声は再送しないデータグラム。TCP と違い、落ちたパケットの再送を待つのはそのストリームだけ
- **ライブ音声** — 中継サーバで聞いている人がいるチャンネルだけ、Opus(既定 16kbps/チャンネル)で送る。誰も聞いていなければ 0。
  1 フレーム(20ms)の欠けは Opus の FEC で復元、5 フレームまでは補間で埋める。中継サーバでミックスして今までどおりの AAC で配信する
- **4G 向け** — 5 秒ごとの keep-alive(NAT が UDP を忘れないように)。会場の IP が変わっても接続は続く(QUIC の接続の移行)
- **mTLS** — TLS 1.3、0-RTT なし。私設 CA は `--gen-uplink-pki` で作る(下)。中継サーバはまだ確かめていない送信元に retry を返す
- **ログイン・Turnstile・国の制限・Tor の拒否** は中継サーバの `[remote]` で設定する(会場の設定は使わない)
- **VPS の構築手順**(Ubuntu 24.04、systemd・AppArmor・seccomp・nftables・chrony NTS・Cloudflare WAF)は
  [deploy/relay/README.md](deploy/relay/README.md)。導入スクリプトは `deploy/relay/install.sh`

### 接続の状態表示

閲覧画面のヘッダの状態ボタンは、どこに問題があるかを色で出す(緑=正常 / 黄=品質低下 / 赤=止まっている / 灰=不明)。
タップすると内訳が出る。

| 行 | 測り方 | 例 |
|---|---|---|
| 端末の回線 | 30 秒ごとに `/cdn-cgi/trace`(Cloudflare のエッジ自身が返す)を取る | 届かない → 赤「端末の回線」 |
| サーバ | `/healthz`(エッジの先のサーバまで届く)と、10 秒ごとの心拍(`event: hb`) | エッジには届くがサーバが応答しない → 赤「サーバ停止」(サーバ側の問題) |
| 会場 | 中継サーバが 10 秒ごとに測る会場との往復時間・未送信数(`event: up`) | 切断 → 赤「会場と切断」、800ms 以上か未送信が溜まる → 黄 |
| 文字起こし | チャンネルの Transcribe 状態 | エラー → 赤 |
| 時刻 | 端末(エッジの時刻と比較)・サーバ(SNTP)・会場(中継サーバとの比較)の時計のずれ | 0.5 秒超 → 黄、2 秒超 → 赤 |
| 文字起こしの遅れ | 話し終えてから画面に出るまで = AWS(話し終え → 会場が結果を受け取る)+ 配信(会場 → 端末)。直近の 1 件 | 4 秒超 → 黄、8 秒超 → 赤 |
| ライブの遅れ | 聞いている間だけ。会場→中継(録った時刻をデータグラムに載せて中継サーバが測る)+ 中継→端末(心拍)+ 再生待ち(端末のバッファと出力) | 1.5 秒超 → 黄、3 秒超 → 赤 |

心拍が 35 秒来なければ、ブラウザが気付かないまま止まった接続とみなしてつなぎ直す(受信済みの位置から続ける)。
直結(Cloudflare を通らない)では「端末の回線」は測れないので灰になり、サーバに届かないときは端末側かサーバ側かを判別できない。

会場の TUI の最下行には、中継サーバとの接続(往復・未送信・会場の時計のずれ)とこの機械の時計のずれを、同じ 4 色で出す。

### 時計のずれの監視(`[clock]`)

会場・中継サーバとも、既定で 64 秒ごとに time.cloudflare.com へ SNTP で問い合わせてこの機械の時計のずれを測る(時計は直さない)。
500ms(`warn_ms`)を超えたらログに警告する。会場の時計がずれていると、閲覧画面の時刻もそのぶんずれる。

### 会場の上り回線の目安

| 送るもの | 直結(上の方式) | 中継サーバ方式 |
|---|---|---|
| 文字起こし・状態 | 閲覧者の数だけ | 1 本分(数 KB/時〜) |
| 区間録音 | 再生した人の数だけ(HE-AAC 24kbps × 人数) | 1 回だけ(発話の長さ × 24kbps) |
| ライブ音声 | 16kbps × 聞いている人数 | 聞かれているチャンネル数 × 16kbps(人数によらない) |
| 回線が戻った直後 | 全員が一斉に再接続して取り直す | 未送信分だけ |

### 1. VPS に中継サーバを置く

東京リージョンの小さな VPS(1 vCPU / 1GB 程度)で足りる。同じリポジトリをビルドして `--relay` で起動する。

```bash
# Ubuntu の例(cpal・fdk-aac・opus のビルドに使うもの。目安)
sudo apt install build-essential cmake pkg-config libasound2-dev
cargo build --release

# 会場 ⇄ 中継の証明書(中継サーバの IP を SAN に入れる)。ca.key はどちらにも置かずオフラインで保管
./target/release/dcr-transcribe --gen-uplink-pki ./secrets/uplink 203.0.113.5

cp relay.toml.example relay.toml   # 必要なら編集
./target/release/dcr-transcribe --relay relay.toml
```

- `--gen-uplink-pki` は `ca.pem` `ca.key` `relay.pem` `relay.key` `venue.pem` `venue.key` を作る。
  中継サーバには `ca.pem` `relay.*`、会場には `ca.pem` `venue.*` を置く
- 会場の QUIC は `[relay] listen`(既定 `[::]:8443`、UDP)。外からは 443/udp で受けて 8443 へ転送するのがおすすめ(4G で通りやすい)。
  **IaaS のファイアウォールで UDP 443 を開ける**(会場の IP は変わるので送信元は絞れない。守りは mTLS)
- 初回起動で `secrets/login_key`(閲覧画面のログインキー)が作られる
- 公開は会場版と同じく `[tunnel]` で VPS 上の cloudflared に任せる。トンネルの転送先は `http://127.0.0.1:8765`。
  Cloudflare を通すので `allowed_countries`・Turnstile もそのまま使える
- ログイン URL は `./target/release/dcr-transcribe --show-login-url relay.toml` で表示できる(会場の TUI の `u` でも、つながったあとは中継サーバの URL が出る)
- ログは標準エラーに出る。常駐させるなら systemd などで:

```ini
# /etc/systemd/system/dcr-relay.service
[Unit]
Description=dcr-transcribe relay
After=network-online.target

[Service]
WorkingDirectory=/opt/dcr-transcribe
ExecStart=/opt/dcr-transcribe/target/release/dcr-transcribe --relay relay.toml
Restart=always

[Install]
WantedBy=multi-user.target
```

### 2. 会場の config.toml に `[uplink]` を追加

```toml
[uplink]
enabled = true
server = "203.0.113.5:443"                  # 中継サーバの IP:ポート(UDP)。DNS に名前を出さなくてよい
ca_file = "./secrets/uplink/ca.pem"
cert_file = "./secrets/uplink/venue.pem"
key_file = "./secrets/uplink/venue.key"
public_url = "https://relay.example.com"    # 閲覧画面の URL(TUI の `u` で QR を出す)
```

- 会場側の `[tunnel]` は不要(止めてよい)。`[remote]` は会場の LAN から見る用に残しておいてもよい
- 区間録音を中継サーバで再生するには、会場の `[segments]` を有効にしておく
- TUI の最下行に `中継: 接続中` / `中継: 切断 N 秒(未送信 文字 3 / 録音 2)` が出る

### トラブルシュート

| 症状 | 対処 |
|------|------|
| TUI が `中継: 切断 …(接続がタイムアウト)` | UDP 443 が届いていない。IaaS のファイアウォール・nftables・会場の回線(UDP を塞いでいないか)を確認 |
| TUI が `中継: 切断 …(接続できません…)` | 証明書の食い違い。会場の `ca.pem` と中継サーバの `ca.pem` が同じ CA か、`server` の IP が relay.pem の SAN に入っているか、期限切れでないか |
| 閲覧画面が「会場と切断中」 | 会場の回線か本体が止まっている。戻れば続きから届く |
| ライブが鳴らない | 会場の `[uplink] live` と、中継サーバの `[live] enabled` を確認。会場のサンプリングレートは 8/12/16/24/48 kHz のどれか(Opus の制約) |
| ▶ が「録音なし」のまま | 区間録音は文字起こしより後に、回線が空いているときに届く。未送信が溜まっていないか TUI で確認 |
| TUI に `時計 +1.2s` など(黄) | 会場の Mac の時計がずれている。システム設定の「日付と時刻」で自動設定を On に |

## macOS SwiftUI アプリ（初版）

macOS 14 以降と Xcode 本体の Swift ツールチェーンでビルドできます。Command Line Tools のみの環境は対象外です。Xcode をインストールし、一度起動して初期設定を完了してください。外部 Swift パッケージは不要です。

```bash
scripts/build_macos.sh
open target/macos/DCRMonitor.app --args --config "$PWD/config.toml"
```

ビルドスクリプトは選択中の Xcode（未選択なら `/Applications/Xcode.app`）の Swift と macOS SDK を使います。システム全体の `xcode-select` 設定は変更しません。別の場所の Xcode は `DEVELOPER_DIR=/path/to/Xcode.app/Contents/Developer scripts/build_macos.sh` で指定できます。
`SwiftUIMacros` が見つからない場合は、Command Line Tools や別配布の Swift と Xcode の SDK が混在していないか確認してください。`xcode-select -p` と `swift --version`、ビルドスクリプトが出力する Xcode・SDK・Swift の情報が調査に使えます。

GUI 起動時に前回の設定（または `--config`）で録音・文字起こしを自動開始します。
初回に設定が未選択ならファイル選択画面を表示します。画面には開始・停止ボタンを置かず、アプリ終了時に録音を確定します。
右上の音声デバイス設定で、入力・出力デバイスと共通の対応サンプルレートを選べます。
保存は音声設定の3項目だけを変更し、他の設定・コメントを保持します。変更は次回起動時に適用され、収録中の音声には影響しません。
デバイスを抜き差しした場合は「再読み込み」で一覧を更新できます。既存の `config.toml` の選択にも対応します。初回開始時には macOS の音声入力許可が必要です。
Rust エンジンをアプリ内へ読み込み、録音・文字起こし・任意の Remote サーバーを同一プロセスで動かします。
別途 TUI を起動したり、GUI で接続先を指定したりする必要はありません。TUI は従来どおり単独でも利用できます。
アプリ終了時はエンジンの停止処理を待ちます。
設定内の相対ファイルパスは設定ファイルの場所を基準に解決し、ログもその隣の `dcr-transcribe.log` に保存します。
録音フォルダは設定から選択し、3秒ごとに再走査します。「録音フォルダを開く」で過去の録音も選択できます。

- 右上のQRアイコンでモバイル版のQRコード・URLを表示し、URLをクリップボードへコピー。稼働中のRemoteサーバーのログインURLを使用し、公開先はTUIと同じ設定から取得します。
- 秒針付きアナログ時計・秒単位デジタル時計。表示メニューで個別に切替。
- 横軸が時刻、縦軸がチャンネルのタイムライン。拡大縮小で表示時間を調整。
- タイムライン上のホイール／横スクロールで過去へ移動。チャンネル行は全行固定表示し、縦には移動しません。
- バーをクリックした位置から音声再生。シーク・速度変更・一時停止。
- 音声ファイルの正常再生終了時は最新の録音位置へ自動復帰。一時停止・エラー時は位置を維持。
- `1`〜`9`: 該当チャンネルの音声出力オン／オフ（収録中）、`←` / `→`: 前後の録音を再生、`Space`: 再生／一時停止、
  `Esc`: 再生を止め最新へ復帰、`⌘O`: フォルダ選択。

緑のバーは区間録音（プリロールや無音の余白を含む）、青は全録音です。発話時間の厳密な集計ではありません。
新規区間録音は音声確定後の `*.aac.json` / `*.wav.json` を索引とします。旧録音はファイル名と
セッションフォルダ名から時刻を推定し、更新後15秒以上経ったファイルを対象とします。
現時点では全録音の `timing.jsonl` を再生位置に反映していないため、入力欠落や時計変更を含む
録音の時刻表示は正確ではありません。再生は選択したファイルの末尾までです。
TUI と同じくチャンネルを横並びにし、各列を「チャンネル名 → 文字起こし → 入力音量 → 接続状態」
の構成にしています。列幅はウィンドウ内で均等分割し、横スクロールは発生させません。
狭い列では本文・チャンネル名を折り返し、状態表示を縦積みにします。
確定結果は緑の時刻、途中結果は黄色の時刻と斜体で区別します。
タイムラインと再生操作は全列共通の小さな領域として下部へ配置しています。
文字起こしは同一プロセスの Rust エンジンから直接取得します。
途中結果はチャンネルごとに更新し、確定結果はGUI側の履歴へ追記して保持します。
GUIを終了・起動しても、確定文字起こし・録音フォルダ・プレイバック用索引を復元します。
保存先は `~/Library/Application Support/DCRMonitor/history.jsonl`。旧版ログの確定文字起こしも初回に取り込みます。
ブラウザ・DCR Monitor とも文字起こしは古い順（新着が下）に表示します。ブラウザはPCではチャンネル別の列、モバイルの「すべて」では全チャンネル混在の1列で表示し、「過去の交信を読み込む」でサーバが保持している以前の履歴を追加できます。過去を読んでいる間は新着への自動スクロールを止め、「最新へ」で再開します。
DCR Monitor のメニューバー「表示」→「表示チャンネル」で文字起こしの列を表示・非表示にできます。非表示の列は画面上に残りません。この選択は保存され、録音・文字起こしには影響しません。
「履歴」メニューで文字起こし／プレイバックの表示履歴を個別にクリアできます。元の音声ファイルは削除しません。
クリア前に始まった録音は再走査しても再表示せず、新しく始まった録音から履歴に加えます。
履歴は取得時に追記・同期し、通常終了時には残りの保存完了を待ちます。
入力音量・VAD・閾値マーカー・Transcribe状態・入力品質警告・保存エラーを表示します。
文字起こしは最新を上に表示します。過去へスクロールすると自動追従を止め、上端まで戻すと再開します。
「最新へ戻る」または再生完了でも追従を再開します。
対応するローカル録音があれば、文字起こし本文をクリックしてその時刻から再生できます。右クリックで本文をコピーできます。
数字キーまたはチャンネル名のクリックで各チャンネルのライブモニターを独立してオン／オフできます。
複数オンの場合は同時刻の入力を平均してミックスし、再生時間の増加や加算による音割れを防ぎます。
プレイバック中はライブモニターを一時ミュートし、再生終了時に元の出力へ戻します。
ファイルをまたぐ連続再生と、欠落位置を考慮した正確な時刻対応は未実装です。

テスト: `cargo build --lib` の後に `swift test --package-path macos --scratch-path target/swift`。
ネイティブ結合テストは存在しない設定を使い、マイクやクラウドを使わず同一プロセス動作と再起動を検証します。

## 入力品質の警告

各チャンネルで入力音声を加工せずに解析し、TUI のチャンネル枠と説明欄に黄色の警告を表示します。
録音・文字起こしは継続します。判定の確定・解除はログと WAV の `*.timing.jsonl` の
`input_quality` イベントに記録します（時刻は異常の発生開始ではなく、判定が確定した時点）。
SwiftUI のタイムライン表示・クリック再生への接続は未実装です。

- ハムの疑い: 50/60 Hz とそれぞれの第2・第3倍音の成分が音声パワーの50%以上、
  音量が -55 dBFS 以上の状態が5秒継続すると警告します。
- 背景雑音増加の疑い: 起動後、VAD 閾値より3 dB以上低くハム候補でない窓を計10秒集め、
  音量の低い側20%点を基準にします。基準より12 dB以上増え、-60 dBFS以上かつ
  VAD 閾値より3 dB以上低い状態が10秒続くと警告します。基準は起動中に追従させません。
- いずれも非該当の音声が3秒続くと解除します。入力が途絶えただけでは解除しません。
  200 ms単位の判定で、入力キューの欠落やコールバック停止をまたいで継続秒数を加算しません。
  解析用の保持量は200 msの音声と最大50個の基準音量です。サンプルレート1 kHz未満は対象外です。

正常に接続した状態で起動してください。起動時から異常な場合や、環境音がVAD閾値を超えて
発話扱いになる場合は、背景雑音増加を見逃します。持続音を含む正常な交信でハム警告が出る
可能性もあります。端子の半挿し・マイク入力への切替を断定する機能ではありません。
漏話のチャンネル間比較や接触不良の瞬間ノイズ判定は未実装です。初期閾値は模擬音声で検証しており、
実機・会場の録音で誤警告と見逃しを評価してから調整してください。

## 録音時刻と MP3 書き出し

全音声 WAV に隣接する `*.timing.jsonl` に、約1秒ごとの音声位置と PC 時計を保存します。
入力サンプル番号はキューから落ちた音声も数えるため、欠落サンプル数を記録できます。
コールバックの時間的な不連続と PC 時計の急な変更も記録します。
時刻は受信時刻からチャンク長を引いた推定値です。デバイス遅延・PC 時計の UTC 誤差・
緩やかな時計補正は保証できません。運用前に OS の時刻同期を確認してください。
入力停止中は時刻記録も増えません。復帰時に時間の不連続を記録しますが、復帰しなかった
場合の停止開始時刻や、デバイス内部で失われたサンプル数は確定できません。
欠落部分に無音は挿入しないため、開始時刻に再生秒数を足すだけでは実時刻になりません。
時刻記録の保存失敗も録音エラーとして画面に表示します。

録音終了後、同じチャンネルの WAV を再生順に指定して、確認・Audacity 編集用の
64 kbps モノラル MP3 を作れます。Python 3 と `libmp3lame` 対応 ffmpeg が必要です。

```bash
python3 scripts/export_mp3.py recordings/channel_0_20261017_*.wav --output review/channel_0
```

出力先は新規ディレクトリに限ります。`review.mp3` と `timeline.json` ができます。
後者には結合位置、元の時刻記録、元 WAV・時刻記録・MP3 の SHA-256 を含めます。
ハッシュは書き出し時点からの変更検出用で、録音日時や真正性の証明ではありません。
MP3 は非可逆なので元の WAV と時刻記録を保存してください。編集後の MP3 には元の
位置対応は適用できません。旧録音など時刻記録のない WAV はこの書き出しでは受け付けません。

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
