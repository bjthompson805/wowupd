#!/bin/sh
# Installs wowupd. Run it as the desktop user, from the checkout; the
# package step goes through doas or sudo. Safe to run again: it's also how an
# updated checkout reaches the machine.
#
#   ./install.sh              packages, build, install
#   ./install.sh --no-system  skip packages (build and install only)
#
# Puts wowupd and wowupd-gtk in ~/.local/bin and a desktop entry in
# ~/.local/share/applications, so desktop launchers list "wowupd". The entry
# names the binary by its full path: a launcher's PATH need not include
# ~/.local/bin.
#
# SPDX-License-Identifier: BSD-2-Clause
set -eu

repo=$(cd "$(dirname "$0")" && pwd)
system=1
for arg; do
	case "$arg" in
	--no-system) system=0 ;;
	*) echo "usage: $0 [--no-system]" >&2; exit 2 ;;
	esac
done
[ "$(id -u)" != 0 ] || { echo "install.sh: run this as your desktop user, not root" >&2; exit 1; }
say() { printf '  %s\n' "$*"; }

if command -v doas >/dev/null 2>&1; then as_root=doas; else as_root=sudo; fi

# ---- packages ----------------------------------------------------------------

packages="gtk4 adwaita-icon-theme rust"
if [ $system -eq 1 ]; then
	missing=
	for p in $packages; do
		pkg info -q "$p" 2>/dev/null || missing="$missing $p"
	done
	if [ -n "$missing" ]; then
		say "installing packages:$missing"
		# shellcheck disable=SC2086
		$as_root pkg install -y $missing
	fi
fi

# ---- build -------------------------------------------------------------------

say "building"
(cd "$repo" && cargo build --release --locked)

# ---- install -----------------------------------------------------------------

bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
mkdir -p "$bin" "$apps"
install -m 755 "$repo/target/release/wowupd" "$repo/target/release/wowupd-gtk" "$bin/"
sed "s|^Exec=wowupd-gtk|Exec=$bin/wowupd-gtk|" "$repo/wowupd.desktop" > "$apps/wowupd.desktop"
chmod 644 "$apps/wowupd.desktop"
say "installed $bin/wowupd, $bin/wowupd-gtk and $apps/wowupd.desktop"

conf="${XDG_CONFIG_HOME:-$HOME/.config}/wowupd/wowupd.conf"
[ -f "$conf" ] || say "no $conf yet: see README.md, Settings"
