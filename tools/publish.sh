#!/bin/sh
# publish.sh PUBLIC_CLONE: put this commit's tree into the public repository's clone as ONE commit.
#
# The development happens here (the private hyperMAC-dev); the public repository gets a snapshot
# per release, without this repository's history (as Android's AOSP gets its code drops). What is
# left out: the paths marked export-ignore in .gitattributes. README.public.md becomes the public
# README.md; this repository's README.md (the developer's notes) goes to docs/DEVELOPING.md.
#
# Contributions: a pull request is merged in the public repository, then brought here at once
# (gh pr diff N | git apply, or git am; keep the author) and its public commit's hash added to
# tools/public-imported.txt. This script refuses to export while the public repository has a
# commit since the last export that is not in that file: the snapshot would undo it.
#
# It commits in the clone and stops: look at the result, then push it yourself.
set -e
D=$(cd "$(dirname "$0")/.." && pwd)
PUB=${1:?usage: publish.sh PUBLIC_CLONE (a clone of the public repository, or a new empty directory)}
die() { echo "publish.sh: $*" >&2; exit 1; }
cd "$D"
[ -z "$(git status --porcelain)" ] || die "uncommitted changes here: commit first"
HEAD=$(git rev-parse HEAD)
VER=$(sed -n 's/^version = "\([0-9.]*\)".*/\1/p' Cargo.toml | head -1)
mkdir -p "$PUB"
PUB=$(cd "$PUB" && pwd)
[ -d "$PUB/.git" ] || git -C "$PUB" init -q -b main
[ -z "$(git -C "$PUB" status --porcelain)" ] || die "$PUB has uncommitted changes"
# every public commit since the last export must have been brought here
if git -C "$PUB" rev-parse -q --verify HEAD >/dev/null; then
	last=$(git -C "$PUB" log --grep='^Exported-From: ' -1 --format=%H)
	for c in $(git -C "$PUB" rev-list --no-merges ${last:+$last..}HEAD); do
		[ "$c" = "$last" ] && continue
		git -C "$PUB" log -1 --format=%B "$c" | grep -q '^Exported-From: ' && continue
		grep -qs "^$c" tools/public-imported.txt ||
		    die "public commit $(git -C "$PUB" log -1 --format='%h %s' "$c") is not in tools/public-imported.txt: bring it here first"
	done
fi
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
git archive HEAD | tar -x -C "$T"
[ -f "$T/README.public.md" ] || die "README.public.md is missing"
mkdir -p "$T/docs"
mv "$T/README.md" "$T/docs/DEVELOPING.md"
mv "$T/README.public.md" "$T/README.md"
rm -f "$T/.gitattributes"
rsync -a --delete --exclude .git "$T/" "$PUB/"
git -C "$PUB" add -A
if git -C "$PUB" diff --cached --quiet; then
	echo "nothing new to publish"
	exit 0
fi
git -C "$PUB" commit -q -m "hyperMAC $VER

Exported-From: $HEAD"
echo "committed in $PUB:"
git -C "$PUB" show --stat --format='%h %s' HEAD | tail -3
echo "look at it, then:  git -C $PUB push"
