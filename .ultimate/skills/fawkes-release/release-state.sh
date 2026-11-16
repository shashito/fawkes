#!/bin/sh
# Answer "released? pushed?" for Fawkes in one short report, and print the commands that would finish the job.
#
#   release-state.sh
#
# Reads only: `git fetch` updates origin/main, and everything else asks git, GitHub (gh) and the local daemons
# (`fawkes status`). It never pushes, tags, releases, installs or restarts; the commands it prints are the
# user's to run.
#
# The checkout is the one this script sits in: run it from the host project, ~/Code/fawkes, where `just
# release` runs. The version is main's Cargo.toml, not the working tree's. The daemons are those of the
# manifests in $FAWKES_HOME (~/.fawkes by default); FAWKES_SOCKET and the other variables an app inherits are
# dropped for `fawkes status`, so inside a supervised app it still reads each manifest's own daemon.
#
# Needs git, gh (logged in) and jq; a missing gh or fawkes shows as "unavailable" and the rest still runs.

set -eu

repo=shashito/fawkes
cd "$(dirname "$0")/../../.."
fawkes_home=${FAWKES_HOME:-$HOME/.fawkes}

have() {
	command -v "$1" >/dev/null 2>&1
}

field() {
	printf '%-13s %s\n' "$1:" "$2"
}

commits() {
	if [ "$1" = 1 ]; then echo "1 commit"; else echo "$1 commits"; fi
}

# fawkes without the variables that would send it to the app's own daemon.
fk() {
	env -u FAWKES_SOCKET -u FAWKES_MANIFEST -u FAWKES_GENERATION -u FAWKES_PHASE fawkes "$@"
}

# ------------------------------------------------ git

fetched=yes
git fetch --quiet origin 2>/dev/null || fetched="no (offline?), origin/main as last fetched"
version=$(git show main:Cargo.toml | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
tag=v$version
ahead=$(git rev-list --count origin/main..main)
behind=$(git rev-list --count main..origin/main)

tag_local=no
if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then tag_local=yes; fi
tag_origin=no
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then tag_origin=yes; fi
since=
if [ "$tag_local" = yes ]; then since=$(git rev-list --count "$tag..main"); fi

field Checkout "$(pwd), branch $(git branch --show-current), $(if [ -z "$(git status --porcelain)" ]; then echo clean; else echo "has changes"; fi)"
field Fetched "$fetched"
field Version "$version (main's Cargo.toml)"
field "Tag $tag" "local $tag_local, origin $tag_origin${since:+, main is $(commits "$since") past it}"
field main "$(commits "$ahead") ahead of origin/main, $behind behind"
if [ "$ahead" -gt 0 ]; then
	git log --oneline --no-decorate origin/main..main | sed 's/^/  /'
fi

# ------------------------------------------------ GitHub

latest=
if have gh; then
	latest=$(gh release list --repo "$repo" --limit 1 --exclude-drafts --json tagName,publishedAt \
		--jq '.[0] | "\(.tagName) \(.publishedAt)"' 2>/dev/null) || latest=
	runs='.[0] | "\(.status) \(.conclusion // "-")  \(.headBranch) \(.headSha[0:7])  \(.createdAt)  \(.url)"'
	ci=$(gh run list --repo "$repo" --workflow ci.yml --branch main --limit 1 \
		--json status,conclusion,headBranch,headSha,createdAt,url --jq "$runs" 2>/dev/null) || ci=
	release=$(gh run list --repo "$repo" --workflow release.yml --limit 1 \
		--json status,conclusion,headBranch,headSha,createdAt,url --jq "$runs" 2>/dev/null) || release=
fi
field Published "${latest:-unavailable}"
field "CI main" "${ci:-unavailable}"
field "Release run" "${release:-unavailable}"
latest_tag=${latest%% *}

# ------------------------------------------------ binary and daemons

installed=
if have fawkes; then installed=$(fk --version | sed 's/^fawkes //'); fi
field Binary "$(if have fawkes; then echo "$(command -v fawkes) $installed"; else echo none; fi)"

# One line per manifest: name, the daemon's version, and a pending switch or a step in progress, which a
# restart would roll back or cut short.
stale=
echo "Daemons:"
for manifest in "$fawkes_home"/*.toml; do
	[ -f "$manifest" ] || continue
	name=$(basename "$manifest" .toml)
	if ! have fawkes; then
		printf '  %-12s unavailable\n' "$name"
	elif status=$(fk status --manifest "$manifest" --json 2>/dev/null); then
		line=$(printf '%s\n' "$status" | jq -r '"\(.version)" + (if .pending then "  pending switch to \(.pending.generation)" else "" end) + (if .busy then "  busy: \(.busy)" else "" end)')
		printf '  %-12s %s\n' "$name" "$line"
		daemon_version=${line%% *}
		if [ -n "$installed" ] && [ "$daemon_version" != "$installed" ]; then
			if [ "$line" = "$daemon_version" ]; then stale="$stale $name"; else stale="$stale $name(busy)"; fi
		fi
	else
		printf '  %-12s no daemon answers\n' "$name"
	fi
done

# ------------------------------------------------ next

echo
echo "Next, for the user (pushing, tagging and releasing are theirs):"
step=0
next() {
	step=$((step + 1))
	echo "  $step. $1"
}
if [ "$behind" -gt 0 ]; then next "git pull --ff-only   # main is behind origin/main"; fi
if [ "$ahead" -gt 0 ]; then next "git push origin main"; fi
if [ "$tag_origin" = no ]; then
	next "just release   # tags $tag and publishes it; refuses unless main is origin/main"
elif [ -n "$latest_tag" ] && [ "$latest_tag" != "$tag" ]; then
	next "wait for the Release run of $tag (gh run view <id> --repo $repo --json status,conclusion)"
fi
if [ -n "$latest_tag" ] && [ "$latest_tag" != "v$installed" ]; then
	next "curl -LsSf https://github.com/$repo/releases/latest/download/install.sh | FAWKES_REQUIRE_ATTESTATION=1 sh"
fi
if [ -n "$stale" ]; then
	if fk reexec --help >/dev/null 2>&1; then
		next "fawkes reexec --all   # daemons on another version:$stale"
	else
		for name in $stale; do
			case $name in
			*'(busy)') next "${name%(busy)}: wait until its switch or step ends, then restart it" ;;
			*) next "fawkes service restart --manifest $name   # from a terminal, not inside the app; restarts the app too" ;;
			esac
		done
	fi
fi
if [ "$step" -eq 0 ]; then echo "  nothing: $tag is pushed, released, installed and running"; fi
if [ "$tag_origin" = yes ] && [ -n "$since" ] && [ "$since" -gt 0 ]; then
	echo "  ($tag is out and main is $(commits "$since") past it: releasing those starts with a version bump in Cargo.toml.)"
fi
