# Recurring tasks for Fawkes: checks, tests, builds and CI. `just` lists them.

default:
    @just --list

# The lander's checks, in its order: fmt, deny, clippy, test. Prints only a failing step's output.
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

# Licences and advisories of the dependencies, against deny.toml.
deny:
    cargo deny check

# A debug build of the fawkes binary.
build:
    cargo build

# Install fawkes from this checkout into ~/.cargo/bin.
install:
    cargo install --path . --locked

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

# Remove this checkout's target/ (about 2 GB).
clean:
    cargo clean
