#!/bin/sh
set -eu
export LC_ALL=C
NAME="${1:-}"
if [ -z "$NAME" ]; then
  echo "usage: nm_inspect.sh CONNECTION_NAME"
  exit 2
fi
echo "name_matches=$(nmcli -g NAME,UUID,TYPE connection show | awk -F: -v n="$NAME" '$1==n{c++} END{print c+0}')"
nmcli -f connection.id,connection.uuid,connection.autoconnect,connection.autoconnect-retries,connection.permissions,802-11-wireless.ssid,802-11-wireless.cloned-mac-address,802-11-wireless.mac-address-randomization,802-11-wireless-security.key-mgmt,802-11-wireless-security.psk-flags connection show "$NAME"
echo "--- device ---"
nmcli -t -f DEVICE,STATE,CONNECTION device status
echo "--- autoconnect-retries docs ---"
man nm-settings-nmcli 2>/dev/null | col -bx | sed -n '/autoconnect-retries/,+12p' | head -n 16 || true
echo NM_INSPECT_OK
