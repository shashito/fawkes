#!/bin/sh
# Roll one Ultimate ref across the Ultimate hosts Fawkes runs on this machine, one host at a time: the canary first,
# the host the caller runs on last, and stop at the first host whose upgrade does not confirm.
#
#   rollout.sh [--plan] [--hosts A,B] [--canary NAME] [--self NAME] [--repo DIR] [--precheck CMD | --no-precheck] <ref>
#
# The ref is resolved once, in the Ultimate clone (--repo, default ~/Code/ultimate), and every host is upgraded to
# that commit. The hosts are the apps of `fawkes status --all` (or, from a fawkes without it, the manifests in
# $FAWKES_HOME) whose manifest's repo is that clone, or the apps --hosts names. The canary (default xana, --canary ''
# for none) goes first; the host whose daemon listens on $FAWKES_SOCKET (or the one --self names) goes last. A host
# already on the commit is left alone, so the same command run again after a failure goes on where it stopped.
#
# The pre-checks run in the foreground and print one line per app: its generation, its commit and its place in the
# order. A host that no daemon answers, or with a switch pending or a step running, refuses the whole rollout before
# anything moves. --plan stops there, with a line on the precheck it would run.
#
# Then the precheck (default ~/Code/ultimate-extensions/fleet/precheck, which type-checks every user extension
# against the Ultimate given in $ULTIMATE): the clone lends a throwaway worktree of the commit (`git worktree add`,
# nothing else in the clone moves), the command runs in it with $ULTIMATE set to its path and $REF to the ref, and
# the worktree is removed whatever the outcome. A failing precheck stops the rollout before any host moves and keeps
# its log. Uncommitted changes in the extensions tree ($PRECHECK_EXTENSIONS, default ~/Code/ultimate-extensions),
# which every host loads live and the precheck would check in place of the ref, refuse the rollout as their own cause.
# --no-precheck skips both.
#
# Otherwise the rollout goes on in the background, in a session of its own, so
# that the switch of the caller's own host, which stops that app's whole process group, does not end it; the command
# prints its status file and returns. The rollout rewrites that file every few seconds: its first line is
# `state: running`, `done` or `failed`, then one line per host (before → after, the outcome, how long it took), and
# its last line is the next command.
#
# Each host runs `fawkes --json upgrade --manifest <name> <commit>` (stage, test, switch), then `fawkes --json wait
# --manifest <name> <gen>` until the generation is confirmed or rolled back, each in the background, polled every
# 3 s. The first failure stops the rollout and adds the failed step, the error's code, cause and message, and the end
# of the generation's log to the status file; the hosts before it stay on the new commit.
#
# It reads the clone with git rev-parse and lends the precheck's worktree from it: it never fetches, merges, pulls
# or checks out there. Needs fawkes, git, jq, and setsid or perl.

set -eu

fawkes_home=${FAWKES_HOME:-$HOME/.fawkes}
script=$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")
poll=3

usage() {
	echo "usage: rollout.sh [--plan] [--hosts A,B] [--canary NAME] [--self NAME] [--repo DIR] [--precheck CMD | --no-precheck] <ref>" >&2
	exit 2
}

die() {
	echo "rollout.sh: $1" >&2
	exit 1
}

# fawkes towards the daemon --manifest names: the variables that would send it to the caller's own app are dropped.
# FAWKES_PHASE stays, so that in a test run fawkes refuses the changes.
fk() {
	env -u FAWKES_SOCKET -u FAWKES_MANIFEST -u FAWKES_GENERATION fawkes "$@"
}

short() {
	if [ -n "$1" ] && [ "$1" != - ]; then printf '%.8s' "$1"; else printf '%s' -; fi
}

duration() {
	printf '%dm%02ds' $(($1 / 60)) $(($1 % 60))
}

# The commit of generation $2 of app $1.
commit_of() {
	fk --json history --manifest "$1" | jq -r --arg g "$2" '.generations[] | select(.id == $g) | .commit'
}

# The repo a manifest names, resolved; empty when it names none or it does not exist.
repo_of() {
	repo=$(sed -n -e 's/^repo *= *"\([^"]*\)".*/\1/p' -e "s/^repo *= *'\([^']*\)'.*/\1/p" "$1" | head -n 1)
	case $repo in
	'') return 0 ;;
	\~/*) repo=$HOME/${repo#"~/"} ;;
	/*) ;;
	*) repo=$(dirname "$1")/$repo ;;
	esac
	(cd "$repo" 2>/dev/null && pwd -P) || true
}

# ------------------------------------------------ the background rollout

# Write the status file whole, then move it into place, so a reader never sees half of it. $1 is the state, $2 the
# next command.
render() {
	{
		printf 'state: %s\n' "$1"
		cat "$out/about"
		cat "$out"/row.*
		if [ -f "$out/detail" ]; then cat "$out/detail"; fi
		printf 'next: %s\n' "$2"
	} >"$out/status.tmp"
	mv "$out/status.tmp" "$out/status"
}

# The host's line: index, name, before, after, outcome, and the seconds it took when known.
row() {
	printf '  %-12s %s → %s  %s%s\n' "$2" "$(short "$3")" "$(short "$4")" "$5" "${6:+  $(duration "$6")}" \
		>"$out/row.$1"
}

# Run the rest of the arguments in the background, stdout to $out/<name>.<step>.json and stderr to .log, and refresh
# the host's line with what its daemon is doing until it ends. Returns its exit code.
follow() {
	step=$1
	shift
	"$@" >"$out/$name.$step.json" 2>"$out/$name.$step.log" &
	pid=$!
	while kill -0 "$pid" 2>/dev/null; do
		doing=$(fk --json status --manifest "$name" 2>/dev/null | jq -r 'if .code then "no answer" else .busy // .step // "waiting" end') ||
			doing="no answer"
		row "$index" "$name" "$before" "" "$step: ${doing:-no answer}" $(($(date +%s) - started))
		render running "$polling"
		sleep "$poll"
	done
	wait "$pid"
}

# Record why the host's step $1 failed: the error, the rollback cause and the end of the generation's log.
failure() {
	json=$out/$name.$1.json
	code=$(jq -r '.code // empty' "$json" 2>/dev/null) || code=
	error=$(jq -r '"\(.code // "error"): \(.message // "")"' "$json" 2>/dev/null) || error=
	[ -n "$error" ] || error="the client printed no answer; $out/$name.$1.log has its output"
	at=$(jq -r '.step // empty' "$json" 2>/dev/null) || at=
	cause=$(jq -r '.cause // empty' "$json" 2>/dev/null) || cause=
	[ -n "$gen" ] || gen=$(jq -r '.generation // empty' "$json" 2>/dev/null) || gen=
	history=$(fk --json history --manifest "$name" 2>/dev/null) || history='{}'
	# The generation the upgrade made: the one the switch answered with, else the newest of the commit.
	record=$(printf '%s\n' "$history" | jq -c --arg g "$gen" --arg c "$target" \
		'[.generations[]? | select(if $g != "" then .id == $g else .commit == $c end)] | last // {}')
	[ -n "$gen" ] || gen=$(printf '%s\n' "$record" | jq -r '.id // empty')
	[ -n "$cause" ] || cause=$(printf '%s\n' "$record" | jq -r '.rollback_cause // empty')
	if [ "$at" = stage ]; then
		what="install output"
		tail=$(printf '%s\n' "$history" | jq -r '.failed_install_tail // empty')
	else
		what="${gen:-its} log"
		tail=$(printf '%s\n' "$record" | jq -r '.log_tail // empty')
	fi
	[ -n "$tail" ] || {
		what="client output"
		tail=$(cat "$out/$name.$1.log")
	}
	current=$(fk --json status --manifest "$name" 2>/dev/null | jq -r '.current // empty') || current=
	after=
	[ -z "$current" ] || after=$(commit_of "$name" "$current") || after=
	if [ "$1" = wait ]; then
		outcome="${code:-not confirmed} $gen${cause:+ ($cause)}"
		headline="$gen did not confirm"
	else
		outcome="${code:-failed} at ${at:-upgrade}${gen:+ ($gen)}"
		headline="its upgrade failed${at:+ at the $at step}"
	fi
	row "$index" "$name" "$before" "$after" "$outcome" $(($(date +%s) - started))
	{
		echo "failed: $name: $headline"
		echo "  error: $error"
		[ -z "$cause" ] || echo "  rollback cause: $cause"
		echo "  $what, last lines:"
		printf '%s\n' "$tail" | tail -n 15 | sed 's/^/    /'
	} >"$out/detail"
}

# The rollout itself: $out, the ref, the commit, then a name and its commit before for each host, in order.
roll() {
	out=$1 ref=$2 target=$3
	shift 3
	polling="sleep 30; cat $out/status"
	state=running
	# A rollout that stops for any other reason (a missing tool, a kill) says so, instead of running forever.
	trap 'if [ "$state" = running ]; then render failed "the rollout stopped before it ended ($out/roll.log may say why); \`fawkes status --all\` shows where the hosts are, and the same command again goes on: $(cat "$out/command")"; fi' EXIT
	trap 'exit 1' HUP TERM
	printf 'rollout of %s (%s) by pid %s, files in %s\n' "$ref" "$(short "$target")" "$$" "$out" >"$out/about"
	count=0
	while [ $# -ge 2 ]; do
		name=$1 before=$2 gen=''
		shift 2
		count=$((count + 1))
		index=$(printf '%02d' "$count")
		started=$(date +%s)
		if follow upgrade fk --json upgrade --manifest "$name" "$target"; then
			gen=$(jq -r '.generation // empty' "$out/$name.upgrade.json")
			if follow wait fk --json wait --manifest "$name" "$gen"; then
				row "$index" "$name" "$before" "$target" "confirmed $gen" $(($(date +%s) - started))
				render running "$polling"
				continue
			fi
			failure wait
		else
			failure upgrade
		fi
		while [ $# -ge 2 ]; do
			count=$((count + 1))
			row "$(printf '%02d' "$count")" "$1" "$2" "" "not started"
			shift 2
		done
		state=failed
		render failed "fawkes history --manifest $name has the rest; once its cause is fixed, the same command again goes on from $name: $(cat "$out/command")"
		exit 1
	done
	state='done'
	render 'done' "nothing: every host runs $(short "$target"); \`fawkes status --all\` shows them"
}

if [ "${1:-}" = --roll ]; then
	shift
	roll "$@"
	exit 0
fi

# ------------------------------------------------ the precheck

# The uncommitted changes of the extensions tree, one `git status --short` line each; empty when it is clean or is
# not a git tree.
unsaved() {
	# No optional locks: an agent may be committing there right now.
	git --no-optional-locks -C "$extensions" status --porcelain 2>/dev/null || true
}

# Take the precheck's worktree back from the clone. A worktree git cannot remove is deleted, and git told to forget it.
return_worktree() {
	[ -d "$lent" ] || return 0
	if ! git -C "$clone" worktree remove --force "$lent" >/dev/null 2>&1; then
		rm -rf "$lent"
		git -C "$clone" worktree prune >/dev/null 2>&1 || true
	fi
}

# Run the precheck against the target in a worktree of it, and stop here unless it passes.
run_precheck() {
	changes=$(unsaved)
	if [ -n "$changes" ]; then
		echo "refused, nothing moved: $extensions has uncommitted changes, which every host loads live and the precheck would check in place of $(short "$target"):"
		printf '%s\n' "$changes" | head -n 10 | sed 's/^/  /'
		echo "next: commit or drop them in $extensions (an agent may be editing it: ask first), then: $command"
		exit 1
	fi
	folder=$(mktemp -d "${tmp%/}/fawkes-precheck.XXXXXX")
	lent=$folder/ultimate
	# Cut short, the precheck leaves nothing behind; only a failed one keeps its log.
	trap 'return_worktree; rm -rf "$folder"' EXIT
	trap 'exit 1' HUP INT TERM
	if ! git -c core.hooksPath=/dev/null -C "$clone" worktree add --quiet --detach "$lent" "$target" >"$folder/log" 2>&1; then
		sed 's/^/  /' "$folder/log"
		die "git worktree add of $(short "$target") in $clone failed, so the precheck could not run; nothing moved"
	fi
	echo "precheck: $precheck, against $(short "$target") in $lent"
	began=$(date +%s)
	if (cd "$lent" && ULTIMATE=$lent REF=$ref sh -c "$precheck") >>"$folder/log" 2>&1; then
		check=0
	else
		check=$?
	fi
	took=$(duration $(($(date +%s) - began)))
	return_worktree
	trap - EXIT HUP INT TERM
	if [ "$check" = 0 ]; then
		rm -rf "$folder"
		echo "precheck passed in $took"
		return 0
	fi
	echo "refused, nothing moved: the precheck failed (exit $check, $took); last lines of $folder/log:"
	tail -n 15 "$folder/log" | sed 's/^/  /'
	echo "next: fix what it names (or the ref), then: $command; --no-precheck skips it"
	exit 1
}

# The precheck takes a while: a host that started a step or a switch meanwhile, or stopped answering, refuses the
# rollout as it would have before the precheck.
recheck() {
	# shellcheck disable=SC2086 # names and commits, one word each
	set -- $order
	while [ $# -ge 2 ]; do
		if ! status=$(fk --json status --manifest "$1" 2>/dev/null); then
			note="no daemon answers any more"
		else
			note=$(printf '%s\n' "$status" | jq -r 'if .step then "a switch is in flight (step \(.step))" elif .busy then "\(.busy) is running" else empty end')
		fi
		if [ -n "$note" ]; then
			echo "refused, nothing moved: during the precheck, $1 changed: $note"
			echo "next: \`fawkes status --manifest $1\`, then once it is free: $command"
			exit 1
		fi
		shift 2
	done
}

# ------------------------------------------------ pre-checks

plan=no hosts='' canary=xana self='' repo=$HOME/Code/ultimate ref=''
precheck=$HOME/Code/ultimate-extensions/fleet/precheck
extensions=${PRECHECK_EXTENSIONS:-$HOME/Code/ultimate-extensions}
tmp=${TMPDIR:-/tmp}
command="sh $script"
while [ $# -gt 0 ]; do
	case $1 in
	--plan) plan=yes ;;
	--no-precheck)
		precheck=
		command="$command $1"
		;;
	--hosts | --canary | --self | --repo | --precheck)
		[ $# -ge 2 ] || usage
		case $1 in
		--hosts) hosts=$(printf '%s' "$2" | tr ',' ' ') ;;
		--canary) canary=$2 ;;
		--self) self=$2 ;;
		--repo) repo=$2 ;;
		--precheck) precheck=$2 ;;
		esac
		command="$command $1 '$(printf '%s' "$2" | sed "s/'/'\\\\''/g")'"
		shift
		;;
	-*) usage ;;
	*)
		[ -z "$ref" ] || usage
		ref=$1
		command="$command '$1'"
		;;
	esac
	shift
done
[ -n "$ref" ] || usage
for tool in fawkes git jq; do
	command -v "$tool" >/dev/null 2>&1 || die "$tool is not on PATH"
done
clone=$(cd "$repo" 2>/dev/null && pwd -P) || die "$repo is not a directory; --repo names the Ultimate clone"
target=$(git -C "$clone" rev-parse --verify --quiet "$ref^{commit}") ||
	die "$ref is not a commit in $clone; fetch it first (git -C $clone fetch), never merge or pull there"

# Every app: the fleet `status --all` knows, else the manifests.
if all=$(fk --json status --all 2>/dev/null); then
	apps=$(printf '%s\n' "$all" | jq -r '.apps[].name')
else
	apps=$(for manifest in "$fawkes_home"/*.toml; do
		if [ -f "$manifest" ]; then basename "$manifest" .toml; fi
	done)
fi
for name in $hosts; do
	[ -f "$fawkes_home/$name.toml" ] || die "--hosts names $name, which has no manifest in $fawkes_home"
done

# The daemon reports its socket with the directories resolved.
self_socket=
if [ -z "$self" ] && [ -n "${FAWKES_SOCKET:-}" ]; then
	if folder=$(cd "$(dirname "$FAWKES_SOCKET")" 2>/dev/null && pwd -P); then
		self_socket=$folder/$(basename "$FAWKES_SOCKET")
	else
		self_socket=$FAWKES_SOCKET
	fi
fi

echo "$ref is $(short "$target") in $clone"
refusals='' first='' middle='' last='' at_target=''
for name in $apps; do
	manifest=$fawkes_home/$name.toml
	[ -f "$manifest" ] || continue
	if [ -n "$hosts" ]; then
		case " $hosts " in *" $name "*) host=yes ;; *) host=no ;; esac
	elif [ "$(repo_of "$manifest")" = "$clone" ]; then
		host=yes
	else
		host=no
	fi
	gen='' commit='' note=''
	if status=$(fk --json status --manifest "$name" 2>/dev/null); then
		gen=$(printf '%s\n' "$status" | jq -r '.current // empty')
		[ -z "$gen" ] || commit=$(commit_of "$name" "$gen")
		step=$(printf '%s\n' "$status" | jq -r '.step // empty')
		busy=$(printf '%s\n' "$status" | jq -r '.busy // empty')
		socket=$(printf '%s\n' "$status" | jq -r '.socket // empty')
		if [ -n "$step" ]; then
			note="refused: a switch is in flight (step $step); \`fawkes wait --manifest $name\` until it is decided"
		elif [ -n "$busy" ]; then
			note="refused: $busy is running; \`fawkes status --manifest $name\` says when it ends"
		fi
		if [ -n "$self_socket" ] && [ "$socket" = "$self_socket" ]; then self=$name; fi
	else
		note="refused: no daemon answers; \`fawkes up --manifest $name\` starts it, or leave it out with --hosts"
	fi
	if [ "$host" = no ]; then
		note="not a host: its repo is not $clone"
	elif [ -n "$note" ]; then
		refusals="$refusals $name"
	elif [ "$commit" = "$target" ]; then
		note="already there"
		at_target="$at_target $name"
	else
		note=upgrade
		# A word for a host with no generation yet, so the order stays pairs of words.
		pair="$name ${commit:--}"
		if [ "$name" = "$self" ]; then
			last=$pair
		elif [ "$name" = "$canary" ]; then
			first=$pair
		else
			middle="$middle $pair"
		fi
	fi
	printf '  %-12s %-4s %-8s  %s\n' "$name" "${gen:--}" "$(short "$commit")" "$note"
done
# The self host may come before the line that finds it; it goes last however it was found.
case " $first$middle " in
*" $self "*)
	without=
	# shellcheck disable=SC2086 # names and commits, one word each
	set -- $first $middle
	while [ $# -ge 2 ]; do
		if [ "$1" = "$self" ]; then last="$1 $2"; else without="$without $1 $2"; fi
		shift 2
	done
	first='' middle=$without
	;;
esac
# shellcheck disable=SC2086,SC2116 # joins the words with single spaces
order=$(echo $first $middle $last)

if [ -n "$refusals" ]; then
	echo "refused, nothing moved: the lines of$refusals say why"
	echo "next: once they are free, or with --hosts naming the others: $command"
	exit 1
fi
if [ -z "$order" ]; then
	echo "nothing to do: no host needs $(short "$target")${at_target:+ (already there:$at_target)}"
	exit 0
fi
# shellcheck disable=SC2086 # the order is names and commits, one word each
set -- $order
names=
while [ $# -ge 2 ]; do
	names="$names $1"
	shift 2
done
echo "order:$names${first:+ (canary ${first%% *})}${last:+ (self ${last%% *})}"
if [ "$plan" = yes ]; then
	if [ -z "$precheck" ]; then
		echo "precheck: none (--no-precheck)"
	elif [ -n "$(unsaved)" ]; then
		echo "precheck: refused now, $extensions has uncommitted changes (\`git -C $extensions status\` lists them)"
	else
		echo "precheck: $precheck, against $(short "$target") in a throwaway worktree, before any host moves"
	fi
	echo "next: $command"
	exit 0
fi
if [ -n "$precheck" ]; then
	run_precheck
	recheck
fi

out=$(mktemp -d "${tmp%/}/fawkes-hosts.XXXXXX")
printf '%s\n' "$command" >"$out/command"
printf 'rollout of %s (%s), starting, files in %s\n' "$ref" "$(short "$target")" "$out" >"$out/about"
count=0
# shellcheck disable=SC2086
set -- $order
while [ $# -ge 2 ]; do
	count=$((count + 1))
	row "$(printf '%02d' "$count")" "$1" "$2" "" waiting
	shift 2
done
render running "sleep 30; cat $out/status"

# A session of its own, away from the caller's process group, which the switch of the caller's own host stops.
# shellcheck disable=SC2086
if command -v setsid >/dev/null 2>&1; then
	setsid sh "$script" --roll "$out" "$ref" "$target" $order </dev/null >"$out/roll.log" 2>&1 &
else
	perl -MPOSIX -e 'fork and exit; POSIX::setsid() or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' \
		sh "$script" --roll "$out" "$ref" "$target" $order </dev/null >"$out/roll.log" 2>&1 &
fi
echo "rolling in the background: $out/status"
echo "next: sleep 30; cat $out/status"
