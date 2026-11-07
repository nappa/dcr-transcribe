#!/usr/bin/env bash
# 中継サーバの導入(Ubuntu 24.04 LTS、root で実行)。何度実行してもよい。
#
#   sudo ADMIN_USER=admin ADMIN_V4=203.0.113.10 [ADMIN_V6=2001:db8::/64] UPLINK_PKI=./uplink ./install.sh ./dcr-transcribe
#
# 前提: このディレクトリ(deploy/relay)と、Linux 向けにビルドした dcr-transcribe を同じ機械に置いてある。
#       UPLINK_PKI に `dcr-transcribe --gen-uplink-pki` で作った ca.pem / relay.pem / relay.key がある(ca.key は持ってこない)。
#       ADMIN_USER の SSH 鍵ログインがすでにできている(パスワードログインを止めるので)。
# 締め出されたら IaaS のコンソール(Linode: LISH / Vultr: View Console)から入って直す。
set -euo pipefail

BIN=${1:?"使い方: $0 <dcr-transcribe のバイナリ>"}
HERE=$(cd "$(dirname "$0")" && pwd)
: "${ADMIN_USER:?ADMIN_USER(SSH で入る管理ユーザ)を指定してください}"
: "${ADMIN_V4:?ADMIN_V4(管理用 PC の IPv4)を指定してください}"
: "${UPLINK_PKI:?UPLINK_PKI(ca.pem / relay.pem / relay.key のあるディレクトリ)を指定してください}"
for f in ca.pem relay.pem relay.key; do
  [[ -s $UPLINK_PKI/$f ]] || { echo "$UPLINK_PKI/$f がありません" >&2; exit 1; }
done
[[ ! -e $UPLINK_PKI/ca.key ]] || { echo "ca.key は中継サーバに置かないでください(オフラインで保管)" >&2; exit 1; }
ADMIN_V6=${ADMIN_V6:-"::1/128"}

[[ $EUID -eq 0 ]] || { echo "root で実行してください" >&2; exit 1; }
. /etc/os-release
[[ ${VERSION_ID:-} == "24.04" ]] || echo "注意: Ubuntu 24.04 以外です (${PRETTY_NAME:-?})。確かめながら進めてください" >&2
id "$ADMIN_USER" >/dev/null || { echo "ユーザ $ADMIN_USER がいません" >&2; exit 1; }
[[ -s /home/$ADMIN_USER/.ssh/authorized_keys ]] || { echo "$ADMIN_USER の authorized_keys がありません(締め出し防止のため中止)" >&2; exit 1; }

step() { printf '\n== %s\n' "$*"; }

step "パッケージ"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get -y upgrade
apt-get -y install chrony nftables apparmor apparmor-utils auditd unattended-upgrades \
  libasound2t64 curl ca-certificates gnupg

step "cloudflared(Cloudflare の apt リポジトリ)"
install -d -m 0755 /usr/share/keyrings
curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg -o /usr/share/keyrings/cloudflare-main.gpg
echo 'deb [signed-by=/usr/share/keyrings/cloudflare-main.gpg] https://pkg.cloudflare.com/cloudflared any main' \
  > /etc/apt/sources.list.d/cloudflared.list
apt-get update
apt-get -y install cloudflared

step "サービス用のユーザ"
id dcr-relay >/dev/null 2>&1 || useradd --system --home-dir /var/lib/dcr-relay --no-create-home --shell /usr/sbin/nologin dcr-relay
id cloudflared >/dev/null 2>&1 || useradd --system --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin cloudflared

step "中継サーバの本体と設定"
install -d -m 0755 /opt/dcr-relay/bin
install -m 0755 -o root -g root "$BIN" /opt/dcr-relay/bin/dcr-transcribe
install -m 0644 "$HERE/README.md" /opt/dcr-relay/README.md
install -d -m 0750 -o root -g dcr-relay /etc/dcr-relay
[[ -e /etc/dcr-relay/relay.toml ]] || install -m 0640 -o root -g dcr-relay "$HERE/etc/relay.toml" /etc/dcr-relay/relay.toml
# 会場との mTLS: 証明書と CA は dcr-relay が読める、鍵は root だけ(systemd が資格情報として渡す)
install -d -m 0750 -o root -g dcr-relay /etc/dcr-relay/tls
install -m 0640 -o root -g dcr-relay "$UPLINK_PKI/ca.pem" /etc/dcr-relay/tls/ca.pem
install -m 0640 -o root -g dcr-relay "$UPLINK_PKI/relay.pem" /etc/dcr-relay/tls/relay.pem
install -m 0600 -o root -g root "$UPLINK_PKI/relay.key" /etc/dcr-relay/tls/relay.key
install -d -m 0700 -o root -g root /etc/cloudflared
if [[ ! -s /etc/cloudflared/tunnel_token ]]; then
  echo "注意: /etc/cloudflared/tunnel_token がありません。ダッシュボードのトンネルのトークンを書いて chmod 600 してください" >&2
fi
# 実行時に要るライブラリがそろっているか
if ldd /opt/dcr-relay/bin/dcr-transcribe | grep -q "not found"; then
  ldd /opt/dcr-relay/bin/dcr-transcribe | grep "not found" >&2
  echo "足りないライブラリがあります" >&2; exit 1
fi

step "時刻(chrony + NTS)"
sed -i -E 's/^(pool |server )/# \1/' /etc/chrony/chrony.conf
install -m 0644 "$HERE/chrony/dcr-nts.sources" /etc/chrony/sources.d/dcr-nts.sources
install -m 0644 "$HERE/chrony/dcr.conf" /etc/chrony/conf.d/dcr.conf
systemctl disable --now systemd-timesyncd 2>/dev/null || true
systemctl enable --now chrony
systemctl restart chrony
# time-sync.target を「実際に同期できた」ことにする(中継サーバはこれを待って起動する)
systemctl enable chrony-wait.service 2>/dev/null || echo "注意: chrony-wait.service がありません" >&2

step "ログ(journald 永続化・30 日)と監査"
install -d /etc/systemd/journald.conf.d
install -m 0644 "$HERE/journald/90-dcr.conf" /etc/systemd/journald.conf.d/90-dcr.conf
systemctl restart systemd-journald
# 改ざん検知の鍵(journalctl --setup-keys)は検証鍵を手元に控える必要があるので、README の手順で手動で作る
install -m 0640 "$HERE/audit/90-dcr.rules" /etc/audit/rules.d/90-dcr.rules
augenrules --load || true
systemctl enable --now auditd

step "カーネルのパラメータ"
install -m 0644 "$HERE/sysctl/90-dcr.conf" /etc/sysctl.d/90-dcr.conf
sysctl --system >/dev/null

step "自動更新(セキュリティだけ、再起動はしない)"
install -m 0644 "$HERE/apt/52-dcr-unattended-upgrades" /etc/apt/apt.conf.d/52-dcr-unattended-upgrades
printf 'APT::Periodic::Update-Package-Lists "1";\nAPT::Periodic::Unattended-Upgrade "1";\n' > /etc/apt/apt.conf.d/20auto-upgrades

step "SSH"
sed "s/^AllowUsers .*/AllowUsers $ADMIN_USER/" "$HERE/ssh/10-dcr-hardening.conf" > /etc/ssh/sshd_config.d/10-dcr-hardening.conf
chmod 0644 /etc/ssh/sshd_config.d/10-dcr-hardening.conf
sshd -t
systemctl reload ssh

step "AppArmor(まず complain: 拒否はせず記録だけ)"
install -m 0644 "$HERE/apparmor/opt.dcr-relay.bin.dcr-transcribe" /etc/apparmor.d/opt.dcr-relay.bin.dcr-transcribe
install -m 0644 "$HERE/apparmor/usr.bin.cloudflared" /etc/apparmor.d/usr.bin.cloudflared
apparmor_parser -r -W /etc/apparmor.d/opt.dcr-relay.bin.dcr-transcribe /etc/apparmor.d/usr.bin.cloudflared
aa-complain /etc/apparmor.d/opt.dcr-relay.bin.dcr-transcribe /etc/apparmor.d/usr.bin.cloudflared

step "systemd のサービス"
install -m 0644 "$HERE/systemd/dcr-relay.service" /etc/systemd/system/dcr-relay.service
install -m 0644 "$HERE/systemd/cloudflared.service" /etc/systemd/system/cloudflared.service
systemctl daemon-reload
systemctl enable --now dcr-relay.service
if [[ -s /etc/cloudflared/tunnel_token ]]; then
  chmod 0600 /etc/cloudflared/tunnel_token
  systemctl enable --now cloudflared.service
fi

step "パケットフィルタ(nftables)"
sed -e "s|^define ADMIN_V4 = .*|define ADMIN_V4 = { $ADMIN_V4 }|" \
    -e "s|^define ADMIN_V6 = .*|define ADMIN_V6 = { $ADMIN_V6 }|" \
    "$HERE/nftables/nftables.conf" > /etc/nftables.conf.new
nft -c -f /etc/nftables.conf.new
mv /etc/nftables.conf.new /etc/nftables.conf
# 読み込んだあと 120 秒以内に Enter が押されなければ元に戻す(締め出し防止)
nft list ruleset > /root/nftables.before
nft -f /etc/nftables.conf
echo "SSH がまだ使えるか、別の端末から入って確かめてください。確かめたら Enter(120 秒で元に戻します)"
if read -r -t 120; then
  systemctl enable nftables
  echo "nftables を有効にしました"
else
  nft flush ruleset
  nft -f /root/nftables.before
  echo "元に戻しました。/etc/nftables.conf を直してからやり直してください" >&2
  exit 1
fi

step "確認"
systemctl --no-pager --full status dcr-relay.service | head -5 || true
systemd-analyze security --no-pager dcr-relay.service | tail -1 || true
chronyc -N tracking | head -4 || true
echo
echo "ログイン URL: sudo -u dcr-relay /opt/dcr-relay/bin/dcr-transcribe --show-login-url /etc/dcr-relay/relay.toml"
echo "会場の QUIC: ss -ulnp | grep 8443(外からは 443/udp。IaaS のファイアウォールで 443/udp を開ける)"
echo "AppArmor の記録を数日見てから enforce に: README の「AppArmor」を参照"
