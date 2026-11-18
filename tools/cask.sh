#!/bin/sh
# cask.sh TAG [TAP_CLONE]: point the Homebrew cask (keelos-dev/homebrew-tap, Casks/hypermac.rb) at
# the release TAG, after `gh release create`. It commits in the tap's clone and stops: push it yourself.
set -e
TAG=${1:?usage: cask.sh TAG [TAP_CLONE]}
TAP=${2:-$HOME/Developer/homebrew-tap}
F=$TAP/Casks/hypermac.rb
VER=$(gh release view "$TAG" -R keelos-dev/hyperMAC --json name -q .name | sed -n 's/^hyperMAC \([0-9.]*\).*/\1/p')
[ -n "$VER" ] || { echo "cask.sh: no release $TAG" >&2; exit 1; }
SHA=$(gh release view "$TAG" -R keelos-dev/hyperMAC --json assets -q '.assets[]|select(.name|endswith(".dmg"))|.digest' | sed 's/^sha256://')
[ -n "$SHA" ] || SHA=$(curl -fsL "https://github.com/keelos-dev/hyperMAC/releases/download/$TAG/$TAG.dmg" | shasum -a 256 | cut -d' ' -f1)
sed -i '' -e "s|^  version \".*\"|  version \"$VER,$TAG\"|" -e "s|^  sha256 \".*\"|  sha256 \"$SHA\"|" "$F"
git -C "$TAP" diff --quiet -- Casks/hypermac.rb && { echo "cask already at $VER"; exit 0; }
git -C "$TAP" commit -q -m "hypermac $VER" -- Casks/hypermac.rb
git -C "$TAP" show --stat --format='%h %s' HEAD | tail -2
echo "look at it, then:  git -C $TAP push"
