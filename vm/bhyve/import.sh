#!/bin/sh
# import.sh SRCTOP: copy the files MANIFEST names from a FreeBSD source tree at the commit in UPSTREAM
# into freebsd/, unchanged. A MANIFEST line "DIR/*.h" takes only the headers under DIR. freebsd/ is never edited by hand: our changes are compat/, hvf/ and patches/.
# To move to a newer upstream: put the new commit in UPSTREAM, check that commit out in SRCTOP, run this,
# and read `git diff --stat freebsd/`.
set -e
D=$(cd "$(dirname "$0")" && pwd)
SRC=${1:?usage: import.sh SRCTOP}
want=$(cat "$D/UPSTREAM")
have=$(git -C "$SRC" rev-parse HEAD)
[ "$want" = "$have" ] || { echo "import.sh: $SRC is at $have, UPSTREAM says $want" >&2; exit 1; }
rm -rf "$D/freebsd"
mkdir -p "$D/freebsd"
grep -v '^#' "$D/MANIFEST" | grep -v '^$' | while read -r p; do
	case $p in
	*/\*.h)
		p=${p%/\*.h}
		[ -d "$SRC/$p" ] || { echo "import.sh: $p is not in $SRC" >&2; exit 1; }
		(cd "$SRC" && rsync -aR -m --include='*/' --include='*.h' --exclude='*' "$p" "$D/freebsd/")
		continue ;;
	esac
	[ -e "$SRC/$p" ] || { echo "import.sh: $p is not in $SRC" >&2; exit 1; }
	(cd "$SRC" && rsync -aR --exclude=/usr.sbin/bhyve/amd64 --exclude=/usr.sbin/bhyve/riscv \
	    --exclude='Makefile.depend*' "$p" "$D/freebsd/")
done
echo "imported $(find "$D/freebsd" -type f | wc -l | tr -d ' ') files at $want"
