#!/bin/sh
# mkpatch.sh upstream|macos NNNN-NAME FILE...: after editing FILEs (paths relative to the FreeBSD tree) in
# build/src, write patches/KIND/NAME.patch = their difference from build/src.new (freebsd/ + the
# patches that already exist). The text above the first "--- a/" of an existing NAME.patch (its
# description) is kept. NNNN is the next free number over both directories: build.sh applies all
# patches as one series in that order. To change an existing patch: delete it, run build.sh, edit, run this.
set -e
D=$(cd "$(dirname "$0")" && pwd)
cd "$D"
kind=${1:?usage: mkpatch.sh upstream|macos NAME FILE...}; name=${2:?NAME}; shift 2
mkdir -p "patches/$kind"
out="patches/$kind/$name.patch"
: > "$out.tmp"
[ -e "$out" ] && sed '/^--- a\//,$d' "$out" > "$out.tmp"
hdr=$(wc -c < "$out.tmp")
for f in "$@"; do
	(cd build && diff -u --label "a/$f" --label "b/$f" "src.new/$f" "src/$f") >> "$out.tmp" || true
done
[ "$(wc -c < "$out.tmp")" -gt "$hdr" ] || { rm -f "$out.tmp"; echo "mkpatch.sh: no differences" >&2; exit 1; }
mv "$out.tmp" "$out"
echo "$out: $(grep -c '^@@' "$out") hunks"
