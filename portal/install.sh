#!/bin/sh
# Install the beewm xdg-desktop-portal routing config (per-user) so screen
# sharing / screen recording works under beewm.
#
# This does NOT install the portal backends themselves — install those with your
# package manager first:
#   Arch:    sudo pacman -S xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk
#   Fedora:  sudo dnf install xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk
#   Debian:  sudo apt install xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk
set -eu

src_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
config_home=${XDG_CONFIG_HOME:-$HOME/.config}

install -Dm644 "$src_dir/beewm-portals.conf" \
  "$config_home/xdg-desktop-portal/beewm-portals.conf"
echo "installed $config_home/xdg-desktop-portal/beewm-portals.conf"

install -Dm644 "$src_dir/xdg-desktop-portal-wlr.conf" \
  "$config_home/xdg-desktop-portal-wlr/config"
echo "installed $config_home/xdg-desktop-portal-wlr/config"

echo
echo "Done. Restart the portal stack (or just log out/in) so it picks up the"
echo "new routing and the session environment beewm exports:"
echo "  systemctl --user restart xdg-desktop-portal xdg-desktop-portal-wlr"
