# 中継サーバの構築(Ubuntu 24.04 LTS / Linode・Vultr 東京)

会場からの QUIC(mTLS)を受け、閲覧画面を配信する中継サーバ(`dcr-transcribe --relay`)を作る。
閲覧者には Cloudflare Tunnel だけで公開し、会場とは cloudflared を通さず QUIC で直接つなぐ。

```
閲覧者(日本国内) ─HTTPS─> Cloudflare(WAF: JP 以外・Tor を拒否)
                                   │ トンネル(VPS から外向きに張る。HTTP の内向きの穴なし)
                                   v
                     [VPS] cloudflared ─> 127.0.0.1:8765 ─┐
                                                          dcr-relay
会場 ══ QUIC 443/udp(mTLS: 私設 CA の証明書どうし)══> :8443 ─┘
                     SSH(22)は管理用 PC の IP からだけ(IaaS のファイアウォール + nftables)
```

会場 ⇄ 中継を QUIC にしているのは、4G でパケットが落ちても待たされるのが落ちたストリームだけで済み
(TCP のように後ろが全部止まらない)、ライブ音声を再送しないデータグラムで送れ、会場の IP が変わっても接続が続くため。

## 守りの層

| 層 | 何で | 内容 |
|---|---|---|
| 閲覧の入口 | Cloudflare WAF | 日本以外(Tor の `T1` を含む)を拒否。ログイン URL の総当たりを回数制限 |
| 会場の入口 | QUIC + mTLS(TLS 1.3) | 私設 CA が署名したクライアント証明書を出せない相手はハンドシェイクで落ちる。送信元の確認(retry)つき。0-RTT は使わない |
| アプリ | `[remote]` | `allowed_countries = ["JP"]`・`block_tor`・`block_private_relay`(iCloud プライベートリレー)・ログインキー・(任意で)Turnstile |
| HTTP | 応答ヘッダ | ハッシュだけの厳しい CSP + Trusted Types、Permissions-Policy、COOP / COEP / CORP、HSTS、`__Host-` + SameSite=Strict のクッキー、Fetch Metadata(README の「D. HTTP の守り」) |
| ネットワーク | IaaS のファイアウォール + nftables | 内向きは SSH(管理用 PC)と QUIC(443/udp)だけ。QUIC の新しい接続は送信元ごとに数を制限。外向きもサービスのユーザごとに宛先ポートを限定 |
| プロセス | systemd | 専用ユーザ・capability なし・seccomp(`SystemCallFilter`)・読み取り専用のファイルシステム |
| プロセス | AppArmor | 読み書きできるファイル・使えるネットワークの種類を限定 |
| SSH | sshd | 鍵だけ・root 不可・ユーザ限定・強い暗号だけ |
| 時刻 | chrony(NTS) | 認証つき NTP。アプリも SNTP でずれを監視して閲覧画面に出す |
| 記録 | journald・auditd | アクセスログ・拒否の記録・設定変更の監査を 30 日保存 |

## OS とクラウドの選び方

**Ubuntu 24.04 LTS を勧める。** Arch Linux は勧めない。

| | Ubuntu 24.04 LTS | Arch Linux |
|---|---|---|
| 更新 | セキュリティ修正だけを自動で入れられる(unattended-upgrades)。2029 年まで(Ubuntu Pro で 2034 年まで) | ローリングリリース。本番の前後に大きな更新が来て動かなくなる恐れがある |
| AppArmor | 最初から有効。systemd・パッケージと一緒に整備されている | 自分でカーネルの起動パラメータから設定する。SELinux は公式に支援されていない |
| cloudflared | Cloudflare の公式 apt リポジトリ(署名つき) | AUR など |
| chrony + NTS | パッケージにある | パッケージにある |
| IaaS のイメージ | Linode・Vultr とも標準で用意 | 用意はあるが更新は自分で |

SELinux は Ubuntu では扱いにくく、RHEL 系(Rocky / Alma)を選ぶならそちらが自然。今回の構成は
「1 つのバイナリを専用ユーザで動かす」だけなので、AppArmor + systemd の sandbox + seccomp で十分に絞れる。

Linode(Akamai)・Vultr はどちらも東京リージョンがあり、無料のクラウドファイアウォールとブラウザから使えるコンソール
(Linode: LISH / Vultr: View Console)がある。どちらでもよい。大きさは 1 vCPU / 1〜2GB で足りる。
コンソールは、SSH から締め出されたときの非常口になる。

## 手順

### 1. VPS を作る

1. 東京リージョン、Ubuntu 24.04 LTS、作成時に自分の SSH 公開鍵(ed25519、できれば FIDO2 の `sk-ssh-ed25519`)を登録する
2. クラウドファイアウォールを作って VPS に付ける
   - 内向き: 既定は **拒否**。TCP 22 を管理用 PC の IPv4(と IPv6)からだけ許可。
     **UDP 443 はすべての送信元から許可**(会場の 4G は IP が変わるので絞れない。守りは mTLS)
   - 外向き: 許可(絞るのは nftables 側)
   - **IPv6 にも同じ規則を入れる**(IPv4 だけ絞って IPv6 が開いている、という事故が多い)
3. 管理用のユーザを作る(以降は root で入らない)

```bash
ssh root@<VPS>
adduser admin && usermod -aG sudo admin
install -d -m 700 -o admin -g admin /home/admin/.ssh
cp ~/.ssh/authorized_keys /home/admin/.ssh/ && chown admin:admin /home/admin/.ssh/authorized_keys
# 別の端末で ssh admin@<VPS> できることを確かめてから先へ
```

### 2. バイナリを作る

本番の VPS にコンパイラやソースを置かないよう、**使い捨てのビルド用 VM**(同じ IaaS の時間課金の小さな VM、Ubuntu 24.04)で作る。

```bash
git clone <このリポジトリ> && cd dcr-transcribe
./deploy/relay/build-linux.sh      # target/release/dcr-transcribe と sha256 が出る
scp target/release/dcr-transcribe admin@<VPS>:
```

### 3. 会場 ⇄ 中継の証明書を作る(手元の PC で)

```bash
# 中継サーバの IP(と、あれば名前)を SAN に入れる。DNS に名前を出さなくても IP で確かめられる
./target/release/dcr-transcribe --gen-uplink-pki ./uplink 203.0.113.5,2001:db8::5
```

| ファイル | 置き場所 |
|---|---|
| `ca.pem` `relay.pem` `relay.key` | 中継サーバ(`install.sh` が /etc/dcr-relay/tls/ に入れる。鍵は root だけが読め、systemd が渡す) |
| `ca.pem` `venue.pem` `venue.key` | 会場の Mac(`./secrets/uplink/`、鍵は 0600) |
| `ca.key` | **どちらにも置かない**。パスワード管理ソフトなどオフラインで保管(作り直すときだけ使う) |

- 鍵は ECDSA P-256、有効期間は既定で 400 日(4 つめの引数で日数を変えられる)
- 私設 CA の証明書は公開の Certificate Transparency ログに載らないので、名前や IP を辿られない。
  会場の宛先は IP で書き、**中継サーバの IP を DNS に出さない**(Cloudflare の陰に隠れた閲覧用の名前と結び付けられないように)
- 会場の Mac を無くした・鍵が漏れたら、CA ごと作り直して両方に入れ直す(失効の仕組みは持たない)

### 4. Cloudflare Tunnel を作る(閲覧用)

Zero Trust → Networks → Tunnels → Create a tunnel(Cloudflared)。

- Public Hostname: `relay.example.com` → Service `http://127.0.0.1:8765`
- 表示されるトークンを VPS の `/etc/cloudflared/tunnel_token` に置く(root:root 0600)。
  コマンドライン(`cloudflared service install <token>`)は使わない(`ps` で見えてしまう)

### 5. 導入スクリプト

```bash
mkdir relay-pki && cp uplink/{ca.pem,relay.pem,relay.key} relay-pki/   # ca.key と venue.* は持っていかない
scp -r deploy/relay relay-pki admin@<VPS>:
ssh admin@<VPS>
sudo install -d -m 700 /etc/cloudflared
sudo sh -c 'cat > /etc/cloudflared/tunnel_token' && sudo chmod 600 /etc/cloudflared/tunnel_token   # トークンを貼って Ctrl-D
sudo ADMIN_USER=admin ADMIN_V4=<管理用 PC の IPv4> ADMIN_V6=<IPv6/64、無ければ省略> UPLINK_PKI=./relay-pki \
  ./relay/install.sh ./dcr-transcribe
rm -r ./relay-pki   # 鍵の写しを残さない
```

スクリプトがやること: パッケージの更新、cloudflared(公式 apt)、専用ユーザ `dcr-relay` / `cloudflared`、
本体と設定の配置、chrony(NTS)、journald(永続・30 日)、auditd、sysctl、自動更新(セキュリティのみ・再起動なし)、
SSH の締め付け、会場との mTLS の証明書、AppArmor(**complain** で読み込み)、systemd のサービス、
nftables(443/udp → 8443 の転送を含む)。
nftables は読み込んでから 120 秒以内に Enter を押さないと元に戻す(締め出し防止)。その間に別の端末から SSH できるか確かめる。

### 6. Cloudflare のダッシュボード(閲覧用)

**WAF → Custom rules**(上から順に):

| 名前 | 式 | 動作 |
|---|---|---|
| Tor を拒否 | `(ip.src.country eq "T1")` | Block |
| 日本以外を拒否 | `(ip.src.country ne "JP")` | Block |
| 不要なメソッドを拒否 | `not (http.request.method in {"GET" "HEAD" "POST"})` | Block |

**WAF → Rate limiting rules**: `(http.request.uri.query contains "k=")` を同じ IP から 10 秒に 10 回まで → Block(ログインキーの総当たり対策)

**iCloud プライベートリレー**: 無料プランの WAF では判定できない(専用の項目が無く、カスタムの IP リストも 1 万件までで
Apple の出口 IP 約 28 万件が入らない)。中継サーバの `block_private_relay = true` で、本体が Apple の一覧を 1 日ごとに取ってきて止める
(外向きの 443 を使う。nftables は dcr-relay の 443 を許可済み)。

**ドメインの設定**:
- SSL/TLS → Edge Certificates → **Encrypted ClientHello (ECH)**: On(会場の Wi-Fi などから、どのサイトを見ているかのホスト名を隠す)
- DNS → Settings → **DNSSEC**: 有効にし、表示される DS レコードをレジストラに登録する(DNS の偽装を防ぐ)
- DNS → Records に **CAA** を足す(Cloudflare が証明書を頼む認証局だけに発行を許す):
  `0 issue "letsencrypt.org"`・`0 issue "pki.goog"`・`0 issue "ssl.com"`・`0 issuewild ";"`(ワイルドカードは誰にも出させない)・
  `0 iodef "mailto:<自分のアドレス>"`。Cloudflare の Universal SSL が使う認証局は変わることがあるので、ダッシュボードの説明で確かめる
- Network → **Network Error Logging**: Cloudflare 自身の NEL は Cloudflare の集計に送られる。本体も NEL を出す(`/reports` へ)。
  `curl -sI https://relay.example.com/healthz | grep -i -E '^(nel|report-to)'` で本体のものが出ているか確かめ、
  Cloudflare のものに置き換わっていたら Cloudflare 側を Off にする

**ほかの設定**:
- Network → Onion Routing: Off
- Speed → Protocol Optimization: HTTP/3 (with QUIC)・0-RTT を On(閲覧者の細い回線向け)
- SSL/TLS → Edge Certificates: Always Use HTTPS、最小 TLS 1.2、HSTS
- bot 対策は Turnstile(`[remote] turnstile_*`)で行う。Bot Fight Mode は閲覧画面の SSE・音声の取得を止めることがあるので使わない
- Rocket Loader・Email Obfuscation・Automatic HTTPS Rewrites・Cloudflare Zaraz / Web Analytics の自動挿入は Off
  (HTML や script を書き換え・挿入すると、CSP のハッシュと合わなくなって画面が動かない。CSP 違反は `/csp-report` のログに出る)
- Transform Rules などで応答ヘッダを足さない(本体が CSP などをすべて出している)

外形監視(UptimeRobot など)で `https://relay.example.com/healthz` を見るなら、監視元が日本以外だと WAF で止まる。
日本の監視拠点を選ぶか、監視元の IP だけ通す規則を「日本以外を拒否」より上に足す。

### 7. 会場の設定

```toml
[uplink]
enabled = true
server = "203.0.113.5:443"                     # 中継サーバの IP(DNS に名前を出さない)
ca_file = "./secrets/uplink/ca.pem"
cert_file = "./secrets/uplink/venue.pem"
key_file = "./secrets/uplink/venue.key"
public_url = "https://relay.example.com"       # 閲覧画面の URL(TUI の `u` で QR が出る)
```

閲覧画面のログイン URL は `sudo -u dcr-relay /opt/dcr-relay/bin/dcr-transcribe --show-login-url /etc/dcr-relay/relay.toml` で分かる
(会場の TUI の `u` でも、つながったあとは出る)。

会場の機材なしで試すなら、手元で疑似の会場を動かす:

```bash
cargo run --release --example fake_venue -- 203.0.113.5:443 ./uplink   # ca.pem / venue.pem / venue.key のあるディレクトリ
```

会場の回線で UDP が通るかは、本番前に必ずその回線(同じ 4G ルータ・SIM)で確かめる。
TUI が `中継: 切断 … (接続がタイムアウト)` のままなら UDP が塞がれている。

### 8. AppArmor を enforce にする

complain のまま会場から送り、閲覧画面で再生・ライブ・共有を一通り使ってから、拒否されるはずだったものを確かめる:

```bash
sudo journalctl -k -g 'apparmor="ALLOWED"' --since today
```

必要なものならプロファイルに足し(`sudo aa-logprof` でも可)、出なくなったら enforce にする:

```bash
sudo aa-enforce /etc/apparmor.d/opt.dcr-relay.bin.dcr-transcribe /etc/apparmor.d/usr.bin.cloudflared
sudo systemctl restart dcr-relay cloudflared
sudo aa-status | grep -E 'dcr-relay|cloudflared'
```

### 9. ログの改ざん検知(任意)

```bash
sudo journalctl --setup-keys     # 表示される検証鍵を VPS の外(パスワード管理ソフトなど)に控える
sudo journalctl --verify --verify-key=<控えた鍵>
```

## 時刻

閲覧画面の時刻は「会場の時計で測った経過」を「中継サーバの時計」に載せて出すので、**両方の時計**が合っている必要がある。

- VPS: chrony が NTS(time.cloudflare.com ほか 2 つ)で合わせる。2 つ以上が一致したときだけ信用する
- 会場の Mac: システム設定 → 一般 → 日付と時刻 →「自動で設定」を On
- アプリ: 中継サーバ・会場とも、`[clock]` の設定で 64 秒ごとに time.cloudflare.com へ SNTP で問い合わせてずれを測り、500ms を超えたらログに警告する
- 中継サーバは 10 秒ごとに会場との往復時間と会場の時計のずれを測る
- 閲覧画面のヘッダの状態ボタン →「時刻」に、端末・サーバ・会場のずれが出る(0.5 秒超で黄、2 秒超で赤)
- 会場の TUI の最下行にも、自分の時計と中継との往復・会場の時計のずれが出る

```bash
chronyc -N tracking          # System time のずれ、Leap status: Normal
chronyc -N authdata          # NTS: KeyID・Cookies が入っていれば鍵交換できている
journalctl -u dcr-relay -g '時計'
```

## ログ

| 何 | 見方 |
|---|---|
| アクセスログ(1 リクエスト 1 行の JSON) | `journalctl -u dcr-relay -o cat -g ' access\] '` |
| 特定の IP | `journalctl -u dcr-relay -o cat -g '"ip":"198.51.100.7"'` |
| 拒否(403)だけ | `journalctl -u dcr-relay -o cat -g '"st":403'` |
| 会場の接続・切断・アドレスの変化 | `journalctl -u dcr-relay -g '会場'` |
| 証明書の無い接続(ポートスキャンなど) | `journalctl -u dcr-relay -g 'ハンドシェイクに失敗'`(1 分ごとの件数) |
| トンネル | `journalctl -u cloudflared` |
| パケットフィルタで落ちたもの | `journalctl -k -g 'nft-(in|out)-drop'` |
| AppArmor | `journalctl -k -g apparmor=` |
| 設定・秘密の変更、root の操作 | `sudo ausearch -k dcr-config -k dcr-secrets -k root-exec -i` |
| SSH | `journalctl -u ssh` |
| Cloudflare で止めたもの | ダッシュボード → Security → Events |

アクセスログの項目: `m` メソッド、`p` パス(ログインキー `k` と共有 URL `u` は `***`)、`st` 状態コード、
`ms` 応答までの時間、`ip`(`CF-Connecting-IP`)、`cc` 国、`ray`(Cloudflare の Ray ID、Security Events と突き合わせられる)、
`site` / `mode` / `dest`(Sec-Fetch-*。無ければ null)、`ua`。

iPhone の Safari で録音・ライブの要求に Sec-Fetch-* が付いているかは、次で確かめる(全部に値が入っていれば、音声の例外を外せる):

```bash
journalctl -u dcr-relay -o cat -g '"p":"/(audio/|live)' | grep -i iphone | grep -o '"site":[^,]*,"mode":[^,]*,"dest":[^,]*' | sort | uniq -c
```
`/healthz` は閲覧画面が 30 秒ごとに叩くので記録しない。IP アドレスは個人情報にあたるので、保存は 30 日(journald の設定)。

## SSH の許可 IP について

自宅の回線は IP が変わることがあり、本番当日に会場(4G)から入ろうとすると許可した IP と違って入れない。決めておくこと:

- **非常口**: IaaS のコンソール(LISH / View Console)。スマホのブラウザからも使える。ログインには管理ユーザのパスワードが要るので、
  `sudo passwd admin` で強いパスワードを付けておく(SSH のパスワードログインは無効のまま。コンソールだけで使える)
- **許可 IP の差し替え**: IaaS の管理画面か API(Linode CLI / Vultr CLI)でファイアウォールの規則を変える。
  手順と認証情報を当日持ち歩く端末で使えるようにしておく。nftables の `ADMIN_V4` も合わせて変える
  (`sudo sed -i 's/^define ADMIN_V4 = .*/define ADMIN_V4 = { 新しい IP }/' /etc/nftables.conf && sudo nft -f /etc/nftables.conf`)
- 代わりの方法: Tailscale(SSH の穴を完全に閉じて tailnet 内だけにする)や、Cloudflare Access の SSH(ブラウザから)。
  どちらも「どこからでも入れる」代わりに、その仕組み自体を信頼することになる

## 本番の前後

**前日まで**
- `sudo apt update && sudo apt upgrade` → `sudo reboot`。再起動後に `systemctl status dcr-relay cloudflared chrony` がすべて active
- `systemd-analyze security dcr-relay` の結果を控える(数字が小さいほど堅い)
- 会場の機材をつないで、閲覧画面の状態ボタンがすべて緑になること、会場の回線を抜くと「会場と切断」(赤)になり、戻すと続きから届くことを確かめる
- 証明書の期限を確かめる(`openssl x509 -enddate -noout -in /etc/dcr-relay/tls/relay.pem`、会場の venue.pem も)
- `chronyc -N tracking` のずれが数 ms 以内
- スナップショット(IaaS の機能)を取る

**当日**
- 自動更新はセキュリティ修正だけで再起動はしないが、気になるなら `sudo systemctl stop unattended-upgrades` で止めておき、終わったら戻す
- 状態の確認は閲覧画面の状態ボタン(端末・サーバ・会場・文字起こし・時刻)と、会場の TUI の最下行

**終わったら**
- ログインキーを作り直す(`/var/lib/dcr-relay/secrets/login_key` を消して `sudo systemctl restart dcr-relay`)
- 会場 ⇄ 中継の証明書も、次に使うときは CA ごと作り直す
- 使わないなら VPS を止める・消す(録音は会場側に原本がある)

## 限界と、ほかに考えること

- **中継サーバは 1 台**。落ちたら systemd がすぐ起動し直すが、履歴はメモリにあるので消える。
  会場は手元の直近 5000 件と区間録音 1000 件を送り直すので、ほとんどは戻る。機械ごと落ちたときに備えるなら、
  スナップショットから別の VPS を作れるようにしておく(トンネルのトークンを入れれば同じ URL で動く)
- 区間録音は `/var/lib/dcr-relay/relay-data/` に起動ごとに溜まる。消すのは手動
- **閲覧側の DDoS は Cloudflare が受けるが、会場の QUIC(443/udp)は VPS の IP に直接来るので Cloudflare の守りが効かない**。
  IP を DNS に出さないこと、nftables の送信元ごとの制限、IaaS の DDoS 対策(Linode は標準、Vultr は有料の追加)で備える。
  狙われて会場の口が使えなくなっても、閲覧画面と記録はトンネル経由で見られる
- 会場の回線で UDP が塞がれていると会場はつながらない(TCP への切り替えは持たない)
- Ubuntu Pro(個人は 5 台まで無料)を有効にすると Livepatch でカーネルの修正を再起動なしで入れられる
- ログを VPS の外に送れば(Grafana Cloud・Better Stack など)、入られてログを消されても残る

## UFW を維持して中継だけ導入する場合

既存サーバで UFW が動いている場合は、全体の `install.sh` を実行せず、
`nftables/ufw-relay.nft` と `systemd/dcr-relay-network.service` を使う構成も取れる。
前者を `/etc/dcr-relay/ufw-relay.nft` に置き、後者を systemd に登録する。
`dcr-relay.service` の drop-in で `Requires=dcr-relay-network.service` と
`After=dcr-relay-network.service` を指定し、両サービスを有効にする。

UFW では `ufw allow in proto udp to any port 8443` を設定する。
追加の nftables テーブルが外部 UDP 443 を 8443 に転送し、外部から直接 8443 に
来た通信を拒否する。Vultr 側の受信許可は引き続き UDP 443 のみでよい。
既存の SSH・UFW ルールは維持する。この構成では、全体の導入スクリプトが行う
SSH 制限・AppArmor・監査・chrony の設定は別途行う。

## 2026-11-06 の導入確認

`transcriber.bunfree.net` の中継を Ubuntu 26.04.1 LTS / x86_64 に配置した。
専用の `dcr-build` ユーザーで `cargo build --release --locked --bin dcr-transcribe -j 2`
を実行し、`/opt/dcr-relay/bin/dcr-transcribe` へ配置。ビルド用ソースと Rust は
`/home/dcr-build/` に残してある。実行ユーザーは別の `dcr-relay`。

- バイナリ SHA-256: `ee00c5833f4162f2af1d1b92c09efb04aeda8559100652982acec47ccd19e8d4`
- 同日、ログイン画面の CSP が `/login` を遮断する不具合を修正して再配置。
  配信中の CSP と HTTPS のセッション発行を確認済み。
- 設定: `/etc/dcr-relay/relay.toml`
- `dcr-relay`、`dcr-relay-network`、`cloudflared` を起動・自動起動設定済み。
- 上記の UFW 併用構成を利用。SSH の設定は変更していない。
- localhost の `/healthz`、外部から UDP 443 への mTLS 接続、文字起こし到着、
  未認証の履歴取得拒否と認証後の取得、サービス再起動を確認済み。
- 待機時メモリ約60MB。負荷試験とライブ音声の実機試験は未実施。
- 公開 URL の curl は Cloudflare のチャレンジで403。ブラウザの表示確認は別途必要。
- 会場証明書と接続設定は手元の `secrets/relay-20261106/` に保存。
  ログイン URL は同ディレクトリの `login-url.txt` (Git 対象外)。

## 全体の導入スクリプトで未確認の項目

上記は個別導入の確認であり、次は引き続き実機での確認が必要。

- `build-linux.sh` でのビルド(必要なパッケージは目安)
- `install.sh` の通しの実行
- AppArmor のプロファイル(`apparmor_parser` での構文チェックも未実施)。そのため complain から始める
- 全体用の nftables 設定(`nft -c` 未実施。UFW 併用版は確認済み)
- Cloudflare apt リポジトリの Origin 名(`apt-cache policy cloudflared` で確かめ、
  `apt/52-dcr-unattended-upgrades` の `origin=cloudflared` を合わせる)
- nftables の 443/udp → 8443 の転送と、送信元ごとの制限(`add @quic_v4 { ... limit rate over ... }`)の書き方
- QUIC が使うシステムコール(`sendmmsg`・GSO など)が seccomp(`@system-service`)と AppArmor で止められないか
- 実際の 4G 回線での QUIC の通り方(接続の移行を含む)
