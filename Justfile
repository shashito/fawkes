# Recurring tasks for Fawkes: checks, tests, builds and CI. `just` lists them.

default:
    @just --list

# The lander's checks, in its order: fmt, scripts, deny, clippy, test. Prints only a failing step's output.
check:
    scripts/check

# fmt, clippy and only the tests matching <test>, e.g. `just check-one watch::`.
check-one test:
    scripts/check --only {{ quote(test) }}

# The tests, with cargo test's own arguments, e.g. `just test --test core rollback`.
test *args:
    cargo test {{ args }}

# Format the code.
fmt:
    cargo fmt

# Clippy on every target, warnings as errors.
lint:
    cargo clippy --all-targets -- -D warnings

# shellcheck on every shell script, found by its shebang, and actionlint on the workflows.
lint-scripts:
    scripts/lint

# Licences and advisories of the dependencies, against deny.toml.
deny:
    cargo deny check

# A debug build of the fawkes binary.
build:
    cargo build

# Install fawkes from this checkout into ~/.cargo/bin.
install:
    cargo install --path . --locked

# Run fawkes from this checkout (a debug build), e.g. `just fawkes status --json`.
fawkes *args:
    cargo run --quiet -- {{ args }}

# The Ultimate extension's tests and type checks, against the clone in $ULTIMATE (default ~/Code/ultimate).
ext-check:
    ultimate/check

# The rust image has the rustup the job calls. The job needs no Docker of its own, and colima's socket cannot be
# mounted into it, hence `--container-daemon-socket -`.

# The Linux job of .github/workflows/ci.yml under act, natively on arm64 (Docker through colima).
ci:
    act push --job check --matrix os:ubuntu-latest --rm \
        --platform ubuntu-latest=catthehacker/ubuntu:rust-latest --container-architecture linux/arm64 \
        --container-daemon-socket -

# The release archive of <target> and its sha256, in dist/, e.g. `just package aarch64-apple-darwin`.
package target:
    scripts/package {{ quote(target) }}

# install.sh against this machine's archive built here (a release build), in a throwaway HOME; installs nothing for you.
install-local:
    #!/bin/sh
    set -eu
    scripts/package "$(rustc -vV | sed -n 's/^host: //p')"
    home=$(mktemp -d /tmp/fawkes-install.XXXXXX)
    trap 'rm -rf "$home"' EXIT
    env -u FAWKES_INSTALL_DIR HOME="$home" FAWKES_DOWNLOAD_URL="file://$PWD/dist" sh install.sh
    "$home/.local/bin/fawkes" --version

# The published release as a new user installs it, with the curl line, in a throwaway HOME; installs nothing for you.
install-check:
    #!/bin/sh
    set -eu
    home=$(mktemp -d /tmp/fawkes-install.XXXXXX)
    trap 'rm -rf "$home"' EXIT
    curl -fsSL -o "$home/install.sh" https://github.com/shashito/fawkes/releases/latest/download/install.sh
    env -u FAWKES_INSTALL_DIR HOME="$home" sh "$home/install.sh"
    "$home/.local/bin/fawkes" --version

# The release workflow on main without publishing anything: checks, builds and installs the four archives (~3 min).
release-dry-run:
    #!/bin/sh
    set -eu
    gh workflow run release.yml --ref main
    # The run shows up in the list a moment after it is requested.
    sleep 5
    gh run watch --exit-status "$(gh run list --workflow release.yml --event workflow_dispatch --limit 1 --json databaseId --jq '.[0].databaseId')"

# Tag v<version in Cargo.toml> on main and push it, which publishes the release once its checks pass. Asks first.
release:
    #!/bin/sh
    set -eu
    version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
    tag=v$version
    [ "$(git branch --show-current)" = main ] || { echo "release from main" >&2; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "the checkout has changes; commit or stash them first" >&2; exit 1; }
    git fetch --quiet origin
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
        { echo "main is not origin/main; push or pull first, so the tag names a commit GitHub has" >&2; exit 1; }
    if git rev-parse --quiet --verify "refs/tags/$tag" >/dev/null; then echo "$tag exists already" >&2; exit 1; fi
    printf 'Tag %s at %s and push it, which publishes Fawkes %s? [y/N] ' "$tag" "$(git rev-parse --short HEAD)" "$version"
    read -r answer
    [ "$answer" = y ] || exit 1
    git tag -m "Fawkes $version" "$tag"
    git push origin "$tag"
    sleep 5
    gh run watch --exit-status "$(gh run list --workflow release.yml --branch "$tag" --limit 1 --json databaseId --jq '.[0].databaseId')"

# Remove this checkout's target/ (about 2 GB).
clean:
    cargo clean
