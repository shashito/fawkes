#!/bin/sh
# Install Fawkes from a GitHub release: the `fawkes` binary and the Ultimate extension it ships with.
#
#   curl -LsSf https://github.com/shashito/fawkes/releases/latest/download/install.sh | sh
#
# `fawkes` goes to $FAWKES_INSTALL_DIR (default ~/.local/bin), the extension to ~/.local/share/fawkes/ultimate/fawkes.
# FAWKES_DOWNLOAD_URL replaces the release's download folder (a mirror, or a file:// folder of archives). Nothing
# needs sudo and no shell startup file is changed. Running it again replaces both with this release's.

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

main() {
	command -v curl >/dev/null || fail "curl is needed to download Fawkes"
	command -v tar >/dev/null || fail "tar is needed to unpack Fawkes"
	[ -n "${HOME:-}" ] || fail "HOME is not set"

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
