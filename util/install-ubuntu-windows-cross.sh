#!/usr/bin/env bash
set -euo pipefail

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  echo "请用 root 运行，或在前面加 sudo" >&2
  exit 1
fi

apt-get update
apt-get install -y --no-install-recommends \
  binutils-mingw-w64-x86-64 \
  gcc-mingw-w64-x86-64-posix \
  g++-mingw-w64-x86-64-posix

if [[ -n "${SUDO_USER:-}" ]]; then
  sudo -u "$SUDO_USER" bash -lc 'source "$HOME/.cargo/env" 2>/dev/null || true; rustup target add x86_64-pc-windows-gnu'
else
  source "$HOME/.cargo/env" 2>/dev/null || true
  rustup target add x86_64-pc-windows-gnu
fi

echo "已安装 Windows 交叉编译依赖"
