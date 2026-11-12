#!/bin/sh
# Install Fawkes from a GitHub release: the `fawkes` binary and the Ultimate extension it ships with.
#
#   curl -LsSf https://github.com/shashito/fawkes/releases/latest/download/install.sh | sh
#
# `fawkes` goes to $FAWKES_INSTALL_DIR (default ~/.local/bin), the extension to ~/.local/share/fawkes/ultimate/fawkes.
# FAWKES_DOWNLOAD_URL replaces the release's download folder (a mirror, or a file:// folder of archives). Nothing
# needs sudo and no shell startup file is changed. Running it again replaces both with this release's.
#
# The archive must match its sha256. When the GitHub CLI is installed and logged in, a release download must also
# carry the release workflow's attestation (`gh attestation verify`); FAWKES_REQUIRE_ATTESTATION=1 refuses any
# archive whose attestation is not verified.

set -eu

# The release workflow writes the release's tag here, so each release's script installs that release.
version=latest

say() {
	echo "fawkes install: $*"
}

fail() {
	echo "fawkes install: $*" >&2
	exit 1
}

# The release target of this machine. A shell under Rosetta reports x86_64 on Apple silicon, which runs the native
# binary just as well.
target() {
	case $(uname -s) in
	Darwin) os=apple-darwin ;;
	Linux) os=unknown-linux-musl ;;
	*) fail "Fawkes runs on macOS and Linux, not $(uname -s)" ;;
	esac
	case $(uname -m) in
	arm64 | aarch64) arch=aarch64 ;;
	x86_64 | amd64) arch=x86_64 ;;
	*) fail "there is no Fawkes build for $(uname -m); build it from source with cargo (see the README)" ;;
	esac
	if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
		arch=aarch64
	fi
	echo "$arch-$os"
}

sha256() {
	if command -v sha256sum >/dev/null; then
		sha256sum "$1" | cut -d ' ' -f 1
	elif command -v shasum >/dev/null; then
		shasum -a 256 "$1" | cut -d ' ' -f 1
	else
		fail "neither sha256sum nor shasum is installed, so the download cannot be checked"
	fi
}

# Whether $1 is the release workflow's build, by its attestation: signed by release.yml of shashito/fawkes on GitHub's
# runners, and for a release's own script on its tag, so it cannot install another release's archive. Refuses an
# archive that fails verification, and one without an attestation unless it comes from $FAWKES_DOWNLOAD_URL (a local
# build has none). When gh cannot verify (not installed, too old, not logged in, no network), it says so and the
# sha256 check stands alone, unless FAWKES_REQUIRE_ATTESTATION=1.
attestation() {
	if ! command -v gh >/dev/null; then
		unverified "the GitHub CLI (gh) is not installed"
		return
	fi
	# Under `curl | sh` the script is standard input, which nothing it runs may read.
	help=$(gh attestation verify --help 2>&1 </dev/null) || help=
	for flag in --signer-workflow --source-ref --deny-self-hosted-runners; do
		case $help in
		*"$flag"*) ;;
		*)
			unverified "this gh is too old to verify attestations; update it"
			return
			;;
		esac
	done
	set -- "$1" --repo shashito/fawkes --signer-workflow shashito/fawkes/.github/workflows/release.yml \
		--deny-self-hosted-runners
	[ "$version" = latest ] || set -- "$@" --source-ref "refs/tags/$version"
	status=0
	output=$(gh attestation verify "$@" 2>&1 </dev/null) || status=$?
	if [ "$status" = 0 ]; then
		say "verified that $archive was built by shashito/fawkes's release workflow"
		return
	fi
	if [ "$status" = 4 ]; then
		unverified "gh is not logged in (\`gh auth login\`)"
		return
	fi
	# gh exits 1 for a failed verification and a failed request alike, so its last line tells them apart. A failed
	# verification can quote the signer's ref, which whoever pushed it chose, so it is ruled out first, and an error
	# that is not recognized refuses.
	reason=$(printf '%s\n' "$output" | sed '/^$/d' | tail -n 1)
	case $reason in
	"Error: verifying"* | "Error: expected"* | *[Pp]olicy*) ;;
	"Error: HTTP 404:"* | "Error: no attestations found"*)
		if [ -z "${FAWKES_DOWNLOAD_URL:-}" ] || [ "$require" = 1 ]; then
			fail "$archive has no attestation from the release workflow, so it may not be the release's; nothing was installed"
		fi
		say "$archive has no attestation (a local build, or not the release's), so only its sha256 was checked"
		return
		;;
	"Error: HTTP 401:"* | "Error: HTTP 403:"* | "Error: HTTP 429:"* | "Error: HTTP 5"* | *"Sigstore verifier"* | \
		*"dial tcp"* | *"no such host"* | *"connection refused"* | *"network is unreachable"* | *timeout*)
		unverified "gh's request to GitHub or Sigstore failed: $reason"
		return
		;;
	esac
	printf '%s\n' "$output" >&2
	fail "$archive could not be verified as the release's build; nothing was installed"
}

# Attestation cannot be checked, for the reason $1.
unverified() {
	if [ "$require" = 1 ]; then
		fail "cannot verify $archive's attestation, which FAWKES_REQUIRE_ATTESTATION=1 requires: $1; nothing was installed"
	fi
	say "cannot verify $archive's attestation, so only its sha256 was checked: $1"
}

main() {
	command -v curl >/dev/null || fail "curl is needed to download Fawkes"
	command -v tar >/dev/null || fail "tar is needed to unpack Fawkes"
	[ -n "${HOME:-}" ] || fail "HOME is not set"
	require=${FAWKES_REQUIRE_ATTESTATION:-0}
	case $require in
	0 | 1) ;;
	*) fail "FAWKES_REQUIRE_ATTESTATION is 1 (refuse an archive without a verified attestation) or unset, not $require" ;;
	esac

	target=$(target)
	if [ "$version" = latest ]; then
		release=https://github.com/shashito/fawkes/releases/latest/download
	else
		release=https://github.com/shashito/fawkes/releases/download/$version
	fi
	url=${FAWKES_DOWNLOAD_URL:-$release}
	bin=${FAWKES_INSTALL_DIR:-$HOME/.local/bin}
	share=$HOME/.local/share/fawkes
	archive=fawkes-$target.tar.gz

	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	# dash runs the EXIT trap on exit, not when a signal ends it.
	trap 'exit 1' INT TERM

	say "downloading $url/$archive"
	curl -fsSL -o "$tmp/$archive" "$url/$archive" || fail "cannot download $url/$archive"
	curl -fsSL -o "$tmp/$archive.sha256" "$url/$archive.sha256" || fail "cannot download $url/$archive.sha256"
	expected=$(cut -d ' ' -f 1 <"$tmp/$archive.sha256")
	actual=$(sha256 "$tmp/$archive")
	[ "$actual" = "$expected" ] ||
		fail "$archive does not match its checksum (sha256 $actual, expected $expected); nothing was installed, try again"
	attestation "$tmp/$archive"
	tar -xzf "$tmp/$archive" -C "$tmp"
	unpacked=$tmp/fawkes-$target
	[ -f "$unpacked/fawkes" ] && [ -d "$unpacked/ultimate/fawkes" ] || fail "$archive is not a Fawkes release archive"

	previous=
	[ -x "$bin/fawkes" ] && previous=$("$bin/fawkes" --version 2>/dev/null || true)

	# Each is copied beside its place and renamed into it, so a running daemon keeps the binary it started from and
	# nothing is ever half copied. The extension's folder is swapped in two renames: for that moment it is missing.
	mkdir -p "$bin" "$share/ultimate"
	cp "$unpacked/fawkes" "$bin/.fawkes.new"
	chmod 755 "$bin/.fawkes.new"
	mv -f "$bin/.fawkes.new" "$bin/fawkes"
	rm -rf "$share/ultimate/.fawkes.new" "$share/ultimate/.fawkes.old"
	cp -R "$unpacked/ultimate/fawkes" "$share/ultimate/.fawkes.new"
	[ -e "$share/ultimate/fawkes" ] && mv "$share/ultimate/fawkes" "$share/ultimate/.fawkes.old"
	mv "$share/ultimate/.fawkes.new" "$share/ultimate/fawkes"
	rm -rf "$share/ultimate/.fawkes.old"

	installed=$("$bin/fawkes" --version)
	if [ -n "$previous" ]; then
		say "installed $installed to $bin/fawkes (was $previous)"
		say "running daemons keep the version they started with until they restart: \`fawkes service restart --manifest <app>\` for a launchd agent, else \`fawkes down\` and \`fawkes up\`, once \`fawkes status\` shows no pending switch (a restart rolls it back)"
	else
		say "installed $installed to $bin/fawkes"
	fi
	say "installed the Ultimate extension to $share/ultimate/fawkes"
	case :$PATH: in
	*:"$bin":*) ;;
	*) say "$bin is not on your PATH; add it to your shell's startup file: export PATH=\"$bin:\$PATH\"" ;;
	esac
	found=$(command -v fawkes || true)
	if [ -n "$found" ] && [ "$found" != "$bin/fawkes" ]; then
		say "$found comes first on your PATH; remove it (\`cargo uninstall fawkes\` for a cargo install) to run this one"
	fi
	[ -n "$previous" ] || say "set a project up with: node $share/ultimate/fawkes/init.ts <project>"
}

main "$@"
