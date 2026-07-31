#!/usr/bin/env bash
# 同梱物を組み立てる（DESIGN 5.3 のレイアウト）。手で配置すると 0 バイトの
# init.lua を置いてしまう等で「起動はするが契約だけ死ぬ」壊れ方をするため、
# 配置はこのスクリプトで行う。
#
#   scripts/make-bundle.sh <nvim-win64 ディレクトリ> <neovide.exe> [出力先]
#
# <nvim-win64 ディレクトリ> は nvim-win64.zip を展開した中身（bin/ と share/ を持つ）。
# 出力先の既定は target/x86_64-pc-windows-msvc/release。
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  sed -n '2,9p' "$0" >&2
  exit 2
fi

nvim_src=$1
neovide_src=$2
repo=$(cd "$(dirname "$0")/.." && pwd)
dest=${3:-$repo/target/x86_64-pc-windows-msvc/release}

[[ -f $nvim_src/bin/nvim.exe ]] || { echo "not an unpacked nvim-win64 dir: $nvim_src" >&2; exit 1; }
[[ -d $nvim_src/share/nvim/runtime ]] || { echo "nvim runtime missing: $nvim_src/share/nvim/runtime" >&2; exit 1; }
[[ -f $neovide_src ]] || { echo "neovide.exe not found: $neovide_src" >&2; exit 1; }
[[ -f $dest/anywhere-nvim.exe ]] || { echo "build anywhere-nvim.exe first: $dest" >&2; exit 1; }

rm -rf "$dest/runtime" "$dest/nvim" "$dest/neovide"
mkdir -p "$dest/nvim" "$dest/neovide"
cp -r "$repo/crates/anywhere-core/runtime" "$dest/runtime"
cp -r "$nvim_src/bin" "$nvim_src/share" "$dest/nvim/"
[[ -d $nvim_src/lib ]] && cp -r "$nvim_src/lib" "$dest/nvim/"
cp "$neovide_src" "$dest/neovide/neovide.exe"

# 空ファイルを置いていないことをここで確認する。host も起動時に同じ検査をする。
for f in "$dest/runtime/init.lua" "$dest/runtime/lua/anywhere/init.lua"; do
  [[ -s $f ]] || { echo "empty after copy: $f" >&2; exit 1; }
done

echo "bundle ready at $dest"
