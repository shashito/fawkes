#!/bin/sh
# Rehearse `fawkes reexec --all` in a throwaway Fawkes world before it is done for real on the live fleet: a supervisor
# and N sample apps started from one fawkes binary, then moved onto another while their pids are watched.
#
#   rehearse.sh up [N] [--from BINARY] [--state FILE]    N apps (default 2, 1..9) under a supervisor of its own
#   rehearse.sh reexec [--to BINARY] [--state FILE]      install BINARY and run `fawkes reexec --all`; check the pids
#   rehearse.sh down [--state FILE]                      stop the world and remove its directory
#
# `up` makes a directory /tmp/fkr.XXXXXX, the FAWKES_HOME of the rehearsal, and records its path in the state file
# (default /tmp/fawkes-rehearse, so that `reexec` and `down` find it from any shell); while one is recorded, `up` is
# refused: `down` first. Inside, bin/fawkes is a copy of BINARY, and each app s1..sN has a git repo (examples/sample's
# app.js and hooks.js, a VERSION file, one commit), its manifest $FAWKES_HOME/sN.toml with `adopt = true`, a state
# directory and a block of ten ports from 30000-38999 that nothing listens on. The supervisor `bin/fawkes up --all` runs
# in a session of its own with its output in supervisor.log and its pid in supervisor.pid; `up` returns once every
# app runs (at most 120 s).
#
# `reexec` records the supervisor's, each daemon's and each app's pid and version, installs BINARY at bin/fawkes
# (copied beside it, then renamed, so the path never holds half a file), runs `bin/fawkes reexec --all` and prints one
# line per target: its version before → after, and whether the pid stayed. It exits 1 when reexec fails, a pid
# changed, or a version is not the one BINARY prints for `--version`.
#
# `down` sends the supervisor SIGTERM, which stops its daemons and, with them, the apps; it waits for them, kills by
# pid any it recorded that is still alive (and says so), then removes the recorded directory and the state file. It
# touches only the pids this script recorded, and never launchd, `fawkes service`, ~/.fawkes or the live supervisor:
# every FAWKES_* variable is dropped on entry and FAWKES_HOME is the rehearsal's directory in every fawkes call.
#
# BINARY defaults to target/debug/fawkes of the repository this script is in, else `fawkes` on PATH; the output says
# which. Needs git, jq, nc, node and perl (for the time bounds and the session).

set -eu

# Whatever FAWKES_* this shell inherited (from an app Fawkes supervises, say) would send a call to its daemon.
for name in $(env | sed -n 's/^\(FAWKES_[A-Za-z0-9_]*\)=.*/\1/p'); do
	unset "$name"
done

here=$(cd "$(dirname "$0")" && pwd -P)
root=$(cd "$here/../../.." && pwd -P)
state=/tmp/fawkes-rehearse
tab=$(printf '\t')
git_dir=''

usage() {
	echo "usage: rehearse.sh up [N] [--from BINARY] [--state FILE]" >&2
	echo "       rehearse.sh reexec [--to BINARY] [--state FILE]" >&2
	echo "       rehearse.sh down [--state FILE]" >&2
	exit 2
}

die() {
	echo "rehearse.sh: $1" >&2
	exit 1
}

# Run the rest of the arguments for at most $1 seconds: the alarm survives the exec. Exits 142 when it fires.
bounded() {
	limit=$1
	shift
	perl -e 'alarm shift; exec @ARGV or die "exec: $!\n"' "$limit" "$@"
}

# Poll until the rest of the arguments succeed, for at most $1 seconds.
wait_until() {
	limit=$1
	shift
	deadline=$(($(date +%s) + limit))
	until "$@"; do
		[ "$(date +%s)" -lt "$deadline" ] || return 1
		sleep 1
	done
}

# fawkes towards the rehearsal and nothing else.
fk() {
	FAWKES_HOME=$dir "$dir/bin/fawkes" "$@"
}

status_json() {
	bounded 30 env FAWKES_HOME="$dir" "$dir/bin/fawkes" --json status --all
}

# One line per target of a `status --all --json` on stdin: name, daemon pid, app pid, version, tab-separated, `-` for
# what is not known. The supervisor is the first, with its own pid.
targets() {
	jq -r '(.supervisor // empty | ["supervisor", .pid, "-", .version]),
		(.apps[] | [.name, (.status.daemon_pid // "-"), (.status.process.pid // "-"), (.status.version // "-")])
		| @tsv'
}

# The directory the state file records, checked to be one of ours before anything is run or removed in it.
recorded_dir() {
	[ -f "$state" ] || die "no rehearsal is recorded in $state; \`sh $0 up\` starts one"
	dir=$(cat "$state")
	case $dir in
	/tmp/fkr.??????) ;;
	*) die "$state names '$dir', which is not a /tmp/fkr.XXXXXX directory of this script; remove $state if it is stale" ;;
	esac
	fawkes_dir=$HOME/.fawkes
	if [ -d "$dir" ] && [ "$(cd "$dir" && pwd -P)" = "$(cd "$fawkes_dir" 2>/dev/null && pwd -P || echo none)" ]; then
		die "$dir is $fawkes_dir, the live fleet's home; refusing"
	fi
}

# The fawkes to copy: the repo's debug build, else the one on PATH. Sets $chosen and $chosen_why.
default_binary() {
	if [ -x "$root/target/debug/fawkes" ]; then
		chosen=$root/target/debug/fawkes
		chosen_why="the debug build of this repository"
	elif chosen=$(command -v fawkes); then
		chosen_why="fawkes on PATH; \`cargo build\` in $root makes the debug build the default"
	else
		die "no fawkes to use: run \`cargo build\` in $root, or name one with the binary option"
	fi
}

# $1 is a fawkes binary the user named, or empty: set $chosen and say which was used.
pick_binary() {
	if [ -n "$1" ]; then
		[ -x "$1" ] || die "$1 is not an executable file; name a fawkes binary"
		chosen=$1
		chosen_why="named on the command line"
	else
		default_binary
	fi
	chosen=$(cd "$(dirname "$chosen")" && pwd -P)/$(basename "$chosen")
	version=$("$chosen" --version) || die "$chosen --version failed; is it a fawkes binary?"
	echo "binary: $chosen ($version; $chosen_why)"
}

need() {
	for tool in "$@"; do
		command -v "$tool" >/dev/null 2>&1 || die "$tool is not on PATH"
	done
}

# ------------------------------------------------ up

# Whether the ten ports from $1 are all free on the loopback.
free_block() {
	port=$1
	while [ "$port" -lt $(($1 + 10)) ]; do
		if nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then return 1; fi
		port=$((port + 1))
	done
}

# Print the first port of a free block that is none of $used (words), at random in 30000-38990.
pick_block() {
	tries=0
	while [ "$tries" -lt 200 ]; do
		tries=$((tries + 1))
		number=$(od -An -N2 -tu2 /dev/urandom | tr -d ' ')
		base=$((30000 + number % 899 * 10))
		case " $1 " in *" $base "*) continue ;; esac
		if free_block "$base"; then
			echo "$base"
			return 0
		fi
	done
	die "no free block of ten ports in 30000-38999 after 200 tries; what listens there? \`lsof -nP -iTCP -sTCP:LISTEN\`"
}

git_in() {
	GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null git -c user.name=rehearse -c user.email=rehearse@localhost \
		-c commit.gpgsign=false -C "$git_dir" "$@"
}

# App $1 on the port block $2: its repo with one commit, and its manifest.
make_app() {
	name=s$1 port=$2
	git_dir=$dir/r$1
	mkdir "$git_dir"
	cp "$root/examples/sample/app.js" "$root/examples/sample/hooks.js" "$git_dir/"
	echo "rehearsal-$name" >"$git_dir/VERSION"
	git_in init -q
	git_in add .
	git_in commit -q -m "rehearsal-$name"
	cat >"$dir/$name.toml" <<EOF
name = "$name"
repo = "$dir/r$1"
state_dir = "$dir/t$1"
home = "$dir/h$1"
port = $port
spare_ports = [$((port + 1)), $((port + 9))]
start = "node app.js"
adopt = true
deadline = 30
stop_timeout = 5
min_uptime = 1

[health]
url = "http://127.0.0.1:{port}/"
interval = 0.2
timeout = 20

[hooks]
snapshot = "node hooks.js snapshot {home} {snapshot}"
restore = "node hooks.js restore {snapshot} {home}"
test_copy = "node hooks.js copy {home} {test_home}"
timeout = 20

[env]
APP_PORT = "{port}"
APP_HOME = "{home}"
EOF
}

# Whether the supervisor answers and every one of the $count apps runs under it.
all_running() {
	supervisor_alive || die "the supervisor (pid $(cat "$dir/supervisor.pid")) ended; $dir/supervisor.log says why, and \`sh $0 down\` cleans up"
	status_json >"$dir/status.json" 2>/dev/null &&
		jq -e --argjson n "$count" '.supervisor != null and (.apps | length) == $n
			and all(.apps[]; .status.process.state == "running" and .supervisor.state == "running")' \
			"$dir/status.json" >/dev/null
}

supervisor_alive() {
	kill -0 "$(cat "$dir/supervisor.pid")" 2>/dev/null
}

up() {
	count=2 from='' counted=no
	while [ $# -gt 0 ]; do
		case $1 in
		--from | --state)
			[ $# -ge 2 ] || usage
			if [ "$1" = --from ]; then from=$2; else state=$2; fi
			shift
			;;
		[1-9])
			[ "$counted" = no ] || usage
			count=$1 counted=yes
			;;
		*) usage ;;
		esac
		shift
	done
	if [ -f "$state" ]; then
		die "a rehearsal is recorded in $state ($(cat "$state")); \`sh $0 down\` removes it first"
	fi
	need git jq nc node perl
	[ -f "$root/examples/sample/app.js" ] && [ -f "$root/examples/sample/hooks.js" ] ||
		die "$root/examples/sample has no app.js and hooks.js; run the script from a fawkes checkout"
	pick_binary "$from"

	used=''
	ports=''
	index=1
	while [ "$index" -le "$count" ]; do
		block=$(pick_block "$used")
		used="$used $block"
		ports="$ports $block"
		index=$((index + 1))
	done

	dir=$(mktemp -d /tmp/fkr.XXXXXX)
	printf '%s\n' "$dir" >"$state"
	recorded_dir
	mkdir "$dir/bin"
	cp "$chosen" "$dir/bin/fawkes"
	index=1
	for block in $ports; do
		make_app "$index" "$block"
		index=$((index + 1))
	done

	# A session of its own, so that it outlives this script; perl execs fawkes, so the pid is the supervisor's.
	perl -MPOSIX -e 'POSIX::setsid() or die "setsid: $!\n"; exec @ARGV or die "exec: $!\n"' \
		env FAWKES_HOME="$dir" "$dir/bin/fawkes" up --all </dev/null >"$dir/supervisor.log" 2>&1 &
	echo $! >"$dir/supervisor.pid"
	echo "rehearsal in $dir: supervisor pid $(cat "$dir/supervisor.pid"), $count apps, port blocks of ten from$ports"
	wait_until 120 all_running ||
		die "not every app runs after 120 s; $dir/supervisor.log and \`FAWKES_HOME=$dir $dir/bin/fawkes status --all\` say why; \`sh $0 down\` cleans up"
	# The pids `down` stops when nothing answers any more.
	targets <"$dir/status.json" >"$dir/up.tsv"
	bounded 30 env FAWKES_HOME="$dir" "$dir/bin/fawkes" status --all
	echo "next: sh $0 reexec [--to BINARY]"
}

# ------------------------------------------------ reexec

# Whether the supervisor and every daemon answer.
all_answer() {
	status_json >"$dir/after.json" 2>/dev/null &&
		jq -e '.supervisor != null and all(.apps[]; .status != null)' "$dir/after.json" >/dev/null
}

reexec() {
	to=''
	while [ $# -gt 0 ]; do
		case $1 in
		--to | --state)
			[ $# -ge 2 ] || usage
			if [ "$1" = --to ]; then to=$2; else state=$2; fi
			shift
			;;
		*) usage ;;
		esac
		shift
	done
	need jq perl
	recorded_dir
	[ -x "$dir/bin/fawkes" ] || die "$dir/bin/fawkes is missing; \`sh $0 down\`, then \`sh $0 up\`"
	pick_binary "$to"
	expected=${version#fawkes }

	status_json >"$dir/before.json" || die "\`fawkes status --all\` fails or takes over 30 s; \`sh $0 down\` and \`up\` again"
	targets <"$dir/before.json" >"$dir/before.tsv"
	grep -q "^supervisor$tab" "$dir/before.tsv" || die "no supervisor answers in $dir; $dir/supervisor.log says why"
	[ "$(wc -l <"$dir/before.tsv")" -ge 2 ] || die "no app has a daemon in $dir; \`sh $0 down\`, then \`sh $0 up\`"

	cp "$chosen" "$dir/bin/.fawkes.new"
	mv "$dir/bin/.fawkes.new" "$dir/bin/fawkes"
	echo "installed $chosen at $dir/bin/fawkes; reexec --all:"
	code=0
	bounded 240 env FAWKES_HOME="$dir" "$dir/bin/fawkes" reexec --all >"$dir/reexec.log" 2>&1 || code=$?
	sed 's/^/  /' "$dir/reexec.log"
	wait_until 60 all_answer || {
		echo "rehearse.sh: the supervisor or a daemon does not answer 60 s after reexec; $dir/supervisor.log and the apps' daemon.log say why" >&2
		code=1
	}
	targets <"$dir/after.json" >"$dir/after.tsv" 2>/dev/null || : >"$dir/after.tsv"
	[ "$code" -eq 0 ] || echo "rehearse.sh: reexec --all exited $code" >&2

	failed=$code
	while IFS=$tab read -r name daemon app before_version; do
		now=$(awk -F"$tab" -v n="$name" '$1 == n' "$dir/after.tsv")
		if [ -z "$now" ]; then
			printf '  %-10s no answer after reexec: CHANGED\n' "$name"
			failed=1
			continue
		fi
		after_daemon=$(echo "$now" | cut -f2) after_app=$(echo "$now" | cut -f3) after_version=$(echo "$now" | cut -f4)
		notes=''
		if [ "$after_daemon" = "$daemon" ]; then notes="pid $daemon kept"; else notes="pid $daemon -> $after_daemon CHANGED" failed=1; fi
		if [ "$app" != - ]; then
			if [ "$after_app" = "$app" ]; then notes="$notes, app $app kept"; else notes="$notes, app $app -> $after_app CHANGED" failed=1; fi
		fi
		if [ "$after_version" != "$expected" ]; then notes="$notes, VERSION is not $expected" failed=1; fi
		printf '  %-10s %s → %s  %s\n' "$name" "$before_version" "$after_version" "$notes"
	done <"$dir/before.tsv"
	if [ "$failed" -ne 0 ]; then
		echo "rehearse.sh: the rehearsal failed; the logs are in $dir (supervisor.log, reexec.log, */daemon.log); \`sh $0 down\` removes them" >&2
		exit 1
	fi
	echo "every pid kept and every version is $expected"
	echo "next: sh $0 down"
}

# ------------------------------------------------ down

pid_alive() {
	kill -0 "$1" 2>/dev/null
}

# Whether none of the pids in $survivors lives.
none_alive() {
	for pid in $survivors; do
		if pid_alive "$pid"; then return 1; fi
	done
}

down() {
	while [ $# -gt 0 ]; do
		case $1 in
		--state)
			[ $# -ge 2 ] || usage
			state=$2
			shift
			;;
		*) usage ;;
		esac
		shift
	done
	if [ ! -f "$state" ]; then
		echo "no rehearsal is recorded in $state; nothing to do"
		exit 0
	fi
	recorded_dir
	if [ ! -d "$dir" ]; then
		rm -f "$state"
		echo "$dir is gone already; forgot it"
		exit 0
	fi
	need jq perl

	# The pids to check afterwards: the daemons' and the apps', from what answers now, else from the last look (after
	# a reexec, else at up).
	if [ -x "$dir/bin/fawkes" ] && status_json >"$dir/down.json" 2>/dev/null; then
		targets <"$dir/down.json" >"$dir/down.tsv"
	elif [ -s "$dir/after.tsv" ]; then
		cp "$dir/after.tsv" "$dir/down.tsv"
	elif [ -f "$dir/up.tsv" ]; then
		cp "$dir/up.tsv" "$dir/down.tsv"
	else
		: >"$dir/down.tsv"
	fi
	survivors=$(awk -F"$tab" '$1 != "supervisor" { for (i = 2; i <= 3; i++) if ($i ~ /^[0-9]+$/) print $i }' "$dir/down.tsv")

	supervisor=$(cat "$dir/supervisor.pid" 2>/dev/null) || supervisor=''
	if [ -n "$supervisor" ] && pid_alive "$supervisor"; then
		case $(ps -o command= -p "$supervisor" 2>/dev/null) in
		*"$(basename "$dir")"/bin/fawkes*) ;;
		*) die "pid $supervisor is alive but is not this rehearsal's fawkes (the pid was reused); remove $dir and $state by hand once nothing of it runs" ;;
		esac
		kill -TERM "$supervisor"
		if wait_until 60 not_alive "$supervisor"; then
			echo "supervisor pid $supervisor stopped on SIGTERM"
		else
			echo "supervisor pid $supervisor still ran 60 s after SIGTERM; killing it"
			kill -KILL "$supervisor" 2>/dev/null || :
		fi
	else
		echo "the supervisor (pid ${supervisor:-unknown}) does not run"
	fi

	if [ -n "$survivors" ] && ! wait_until 10 none_alive; then
		for pid in $survivors; do
			if pid_alive "$pid"; then
				echo "pid $pid (a daemon or app of the rehearsal) survived the supervisor; killing it"
				kill -TERM "$pid" 2>/dev/null || :
			fi
		done
		wait_until 10 none_alive || for pid in $survivors; do
			if pid_alive "$pid"; then
				echo "pid $pid ignored SIGTERM; SIGKILL"
				kill -KILL "$pid" 2>/dev/null || :
			fi
		done
	fi
	[ -z "$survivors" ] || echo "recorded pids gone: $(echo "$survivors" | tr '\n' ' ')"
	rm -rf "$dir"
	rm -f "$state"
	echo "removed $dir and $state"
}

not_alive() {
	! pid_alive "$1"
}

# ------------------------------------------------ main

[ $# -ge 1 ] || usage
command=$1
shift
case $command in
up) up "$@" ;;
reexec) reexec "$@" ;;
down) down "$@" ;;
*) usage ;;
esac
