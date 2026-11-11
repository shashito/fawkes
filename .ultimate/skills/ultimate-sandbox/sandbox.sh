#!/bin/sh
# A throwaway Ultimate for checking the Fawkes extension (ultimate/fawkes) end to end, never the live install.
# Everything lives in one folder ($SANDBOX, default /tmp/fxs) and every process runs under `env -i` with a sandboxed
# HOME, ULTIMATE_USER_HOME and FAWKES_HOME, so nothing reads or writes ~/.ultimate, ~/.fawkes or the live hosts' ports.
#
#   sandbox.sh up <fawkes-binary> [<extension-dir>]   build it: Ultimate clone, user home, project, binary, extension
#   sandbox.sh run <command> [args…]                  run a command in the sandbox's environment, in the project
#   sandbox.sh start <name> <command> [args…]         the same in the background; pid in pids/<name>, output in logs/<name>.log
#   sandbox.sh stop <name>                            SIGTERM that process group, wait up to 60 s, then SIGKILL it
#   sandbox.sh down                                   stop everything started here and delete the folder
set -eu
S=${SANDBOX:-/tmp/fxs}
live=${ULTIMATE_LIVE:-$HOME/Code/ultimate}
here=$(cd "$(dirname "$0")" && pwd)

# The sandbox's environment: no inherited ULTIMATE_* or FAWKES_* (the shell of an agent runs inside a live host).
# The real pnpm store, named by its real path: a store path that differs from the one node_modules was installed with
# makes pnpm purge node_modules, which it refuses without a TTY.
store=${PNPM_STORE:-$HOME/Library/pnpm/store}
path="$S/bin:$(dirname "$(command -v node)"):$(dirname "$(command -v pnpm)"):$(dirname "$(command -v git)"):/usr/bin:/bin:/usr/sbin:/sbin"
sandboxed() {
  env -i HOME="$S/user" ULTIMATE_USER_HOME="$S/user" FAWKES_HOME="$S/fh" PATH="$path" LANG=en_US.UTF-8 \
    GIT_CONFIG_GLOBAL=/dev/null GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t \
    GIT_COMMITTER_EMAIL=t@example.com npm_config_store_dir="$store" "$@"
}

stop() {
  pid=$(cat "$S/pids/$1" 2>/dev/null) || return 0
  # Started with `set -m`, so the pid leads its own group: the whole group goes, and only it.
  kill -TERM -- "-$pid" 2>/dev/null || { rm -f "$S/pids/$1"; return 0; }
  i=0
  while kill -0 -- "-$pid" 2>/dev/null && [ $i -lt 120 ]; do sleep 0.5; i=$((i + 1)); done
  kill -KILL -- "-$pid" 2>/dev/null || true
  rm -f "$S/pids/$1"
}

case ${1:-} in
up)
  binary=${2:?usage: sandbox.sh up <fawkes-binary> [<extension-dir>]}
  extension=${3:-$here/../../../ultimate/fawkes}
  [ -e "$S" ] && { echo "$S exists: run \`sandbox.sh down\` first" >&2; exit 1; }
  mkdir -p "$S/bin" "$S/user/.ultimate/extensions" "$S/user/Library/Caches" "$S/fh" "$S/pids" "$S/logs" "$S/proj"
  # pnpm's own copy (the clone pins its version) comes from the real cache, so installs stay offline.
  ln -s "$HOME/Library/Caches/pnpm" "$S/user/Library/Caches/pnpm"
  cp "$binary" "$S/bin/fawkes"
  # A copy, not a link: a landing in the working tree must not change the extension under test.
  cp -R "$extension" "$S/ext"
  # origin.git stands for GitHub: push to it from work/ to give `fawkes up origin/main` something new.
  git clone -q --bare "$live" "$S/origin.git"
  git clone -q "$S/origin.git" "$S/ultimate"
  git clone -q "$S/origin.git" "$S/work"
  # Installed from the real store, in the sandbox's environment: a copy of the live node_modules would make pnpm purge
  # it (it refuses that without a TTY), and generations clone this one (`reuse`).
  (cd "$S/ultimate" && sandboxed pnpm install --frozen-lockfile --prefer-offline >"$S/logs/install.log" 2>&1) ||
    { echo "pnpm install failed: $S/logs/install.log" >&2; exit 1; }
  # No onboarding, no extensions beyond the defaults, no secrets; the Fawkes extension user-wide, as a user links it.
  printf '{ "onboarding": false, "extensions": { "enable": [] } }\n' >"$S/user/.ultimate/settings.json"
  ln -s "$S/ext" "$S/user/.ultimate/extensions/fawkes"
  (cd "$S/proj" && sandboxed git init -q && sandboxed git commit -q --allow-empty -m init)
  echo "sandbox ready in $S (Ultimate $(git -C "$S/ultimate" rev-parse --short HEAD))"
  ;;
run)
  shift
  cd "$S/proj"
  sandboxed "$@"
  ;;
start)
  name=${2:?usage: sandbox.sh start <name> <command> [args…]}
  shift 2
  cd "$S/proj"
  set -m
  sandboxed "$@" >"$S/logs/$name.log" 2>&1 &
  echo $! >"$S/pids/$name"
  echo "$name: pid $!, log $S/logs/$name.log"
  ;;
stop)
  stop "${2:?usage: sandbox.sh stop <name>}"
  ;;
down)
  for file in "$S"/pids/*; do [ -e "$file" ] && stop "$(basename "$file")"; done
  rm -rf "$S"
  echo "removed $S"
  ;;
*)
  sed -n '2,11p' "$0" >&2
  exit 2
  ;;
esac
