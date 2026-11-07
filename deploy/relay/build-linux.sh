#!/usr/bin/env bash
# Linux(Ubuntu 24.04、x86_64)向けに中継サーバのバイナリを作る。
# 中継サーバそのものではなく、使い捨てのビルド用の機械(同じ IaaS で時間課金の小さな VM など)で実行し、
# できたバイナリだけを中継サーバへ scp する(本番の機械にコンパイラやソースを置かない)。
#
#   ./deploy/relay/build-linux.sh   → target/release/dcr-transcribe
set -euo pipefail
cd "$(dirname "$0")/../.."
sudo apt-get update
sudo apt-get -y install build-essential cmake pkg-config libasound2-dev curl
command -v cargo >/dev/null || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
cargo build --release --locked
ls -l target/release/dcr-transcribe
ldd target/release/dcr-transcribe
sha256sum target/release/dcr-transcribe
