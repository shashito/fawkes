#!/bin/sh
# Report the disk Fawkes' Ultimate checkouts and test leftovers take, and with --clean free it.
#
#   free-disk.sh                              report
#   free-disk.sh --clean [--skip <path>]...   also clean, leaving each --skip checkout alone
#
# Reports df for the home, the size of each target/ in the Fawkes worktrees, the lander checkouts and
# ~/Code/fawkes, and the /tmp/fk* and /tmp/fawkes-* folders, then lists the stale /tmp/fk* folders: nothing
# changed in them within a day and no process has them open. --clean runs `cargo clean` in each worktree and
# lander checkout that is not skipped and has no process inside it.
#
# The script deletes nothing itself: cargo removes the build folders, and whoever runs it removes the listed
# stale folders with one `rm -rf <path>` each, a literal path the guard extension can read. It never touches
# ~/Code/fawkes (worktree setup copies its target), /tmp/fawkes-* (the test harness keeps its port claims
# there) or anything outside these paths.
#
# A process is "inside" a path when its working directory, its executable or any file it has open lies under
# it. One `lsof` over all processes answers that in well under a second and sees cargo, rustc, test binaries
# and shells alike; `lsof +D target` would only see open files and walks every file of a target of many
# thousands.
#
# FREE_DISK_HOME and FREE_DISK_TMP replace $HOME and /tmp, for testing on throwaway folders.

set -eu

home=${FREE_DISK_HOME:-$HOME}
tmp=${FREE_DISK_TMP:-/tmp}

usage() {
	echo "usage: free-disk.sh [--clean] [--skip <path>]..." >&2
	exit 2
}

# The physical path of a directory, as lsof reports it (/tmp is /private/tmp on macOS); the path itself when it
# does not exist.
resolve() {
	(cd "$1" 2>/dev/null && pwd -P) || printf '%s\n' "$1"
}

newline='
'
clean=
# The skipped checkouts, each between newlines.
skips=$newline
while [ $# -gt 0 ]; do
	case $1 in
	--clean) clean=1 ;;
	--skip)
		[ $# -ge 2 ] || usage
		skips=$skips$(resolve "$2")$newline
		shift
		;;
	*) usage ;;
	esac
	shift
done

source=$(resolve "$home/Code/fawkes")
open=$(mktemp)
output=$(mktemp)
freed=$(mktemp)
failed=$(mktemp)
trap 'rm -f "$open" "$output" "$freed" "$failed"' EXIT

# Kilobytes taken by a path, 0 when it is gone.
kilobytes() {
	if [ -e "$1" ]; then du -sk "$1" 2>/dev/null | cut -f1; else echo 0; fi
}

human() {
	awk -v k="$1" 'BEGIN {
		if (k >= 1048576) printf "%.1fG", k / 1048576
		else if (k >= 1024) printf "%.0fM", k / 1024
		else printf "%dK", k
	}'
}

# Whether a process is inside directory $1. lsof names a working directory or an open file by its physical
# path but a Unix socket by the path it was bound at, so both are looked for. Without a usable lsof every
# directory counts as busy.
busy() {
	# lsof exits 1 when it cannot read some process, which is normal; no output at all means it did not run.
	lsof -nP -Fn >"$open" 2>/dev/null || true
	[ -s "$open" ] || return 0
	for path in "$1" "$(resolve "$1")"; do
		if grep -q -x -F "n$path" "$open" || grep -q -F "n$path/" "$open"; then
			return 0
		fi
	done
	return 1
}

# Whether checkout $1 is named with --skip, or contains a path that is.
skipped() {
	case $skips in *"$newline$1$newline"* | *"$newline$1/"*) return 0 ;; esac
	return 1
}

checkouts() {
	for checkout in "$home"/.ultimate/projects/fawkes-*/worktrees/*/ "$home"/.ultimate/projects/fawkes-*/lander/*/; do
		if [ -d "$checkout/target" ]; then printf '%s\n' "${checkout%/}"; fi
	done
}

leftovers() {
	for folder in "$tmp"/fk* "$tmp"/fawkes-*; do
		if [ -d "$folder" ] && [ ! -L "$folder" ]; then printf '%s\n' "$folder"; fi
	done
}

echo "Disk:"
df -h "$home"

echo
echo "Targets:"
checkouts | while IFS= read -r checkout; do
	printf '  %6s  %s\n' "$(human "$(kilobytes "$checkout/target")")" "$checkout/target"
done
if [ -d "$home/Code/fawkes/target" ]; then
	printf '  %6s  %s (kept: worktree setup copies it)\n' "$(human "$(kilobytes "$home/Code/fawkes/target")")" "$home/Code/fawkes/target"
fi

echo
echo "Test leftovers:"
leftovers | while IFS= read -r folder; do
	printf '  %6s  %s\n' "$(human "$(kilobytes "$folder")")" "$folder"
done

# Only fk* folders: /tmp/fawkes-test-ports holds the test harness's port claims.
echo
echo "Stale test folders, to remove one by one with rm -rf <path>:"
for folder in "$tmp"/fk*; do
	[ -d "$folder" ] && [ ! -L "$folder" ] || continue
	if [ -z "$(find "$folder" -mtime -1 -print -quit 2>/dev/null)" ] && ! busy "$folder"; then
		printf '%s\n' "$folder"
	fi
done

[ -n "$clean" ] || exit 0

echo
echo "Cleaning:"
checkouts | while IFS= read -r checkout; do
	if [ "$(resolve "$checkout")" = "$source" ]; then
		continue
	elif skipped "$(resolve "$checkout")"; then
		echo "  skipped $checkout: named with --skip"
	elif busy "$checkout"; then
		echo "  skipped $checkout: a process is inside it"
	elif [ ! -f "$checkout/Cargo.toml" ]; then
		echo "  skipped $checkout: no Cargo.toml"
	else
		before=$(kilobytes "$checkout/target")
		# Both pin what cargo removes to this target: CARGO_TARGET_DIR, CARGO_BUILD_BUILD_DIR or a cargo config could
		# point the target or build directory elsewhere, a folder other projects may share.
		if (cd "$checkout" && CARGO_BUILD_BUILD_DIR="$checkout/target" cargo clean --manifest-path Cargo.toml \
			--target-dir "$checkout/target") >"$output" 2>&1; then
			gain=$((before - $(kilobytes "$checkout/target")))
			echo "$gain" >>"$freed"
			echo "  cleaned $checkout: $(human "$gain")"
		else
			echo "$checkout" >>"$failed"
			echo "  failed to clean $checkout:"
			sed 's/^/    /' "$output"
		fi
	fi
done

echo
echo "Freed $(human "$(awk '{ total += $1 } END { print total + 0 }' "$freed")") (by du; APFS clones share blocks, so df shows the real gain)"
df -h "$home"
# A checkout that failed to clean fails the run, after the report.
[ ! -s "$failed" ]
