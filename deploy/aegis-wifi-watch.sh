#!/bin/sh
# Optional. Only activate a saved UUID when it is down. Never bounce a live association
# to recover Cloudflare. No secrets in logs. UUID via EnvironmentFile.
set -eu
export LC_ALL=C
UUID="${AEGIS_WIFI_UUID:-}"
if [ -z "$UUID" ]; then
  echo "AEGIS_WIFI_UUID unset"
  exit 2
fi
LOCK=/run/aegis-wifi-watch.lock
STATE=/run/aegis-wifi-watch.state
exec 9>"$LOCK"
if ! flock -n 9; then
  echo "busy"
  exit 0
fi

nm_state=$(nmcli -t -f UUID,STATE connection show --active 2>/dev/null | awk -F: -v u="$UUID" '$1==u{print $2; found=1} END{if(!found) print "down"}')
case "$nm_state" in
  activated|activating|connecting|deactivating)
    echo "already=$nm_state"
    rm -f "$STATE"
    exit 0
    ;;
esac

wlan=$(nmcli -t -f DEVICE,STATE device status 2>/dev/null | awk -F: '$1=="wlan0"{print $2}')
case "$wlan" in
  *connecting*|*configuring*|*need\ authentication*)
    echo "wlan0_busy=$wlan"
    exit 0
    ;;
esac

now=$(date +%s)
fail_n=0
last=0
if [ -f "$STATE" ]; then
  # fail_n last_unix
  read -r fail_n last <"$STATE" || true
fi
if [ "$fail_n" -gt 8 ]; then
  fail_n=8
fi
# Backoff: 2,4,8… minutes, cap 16 min. Timer is ~2 min so this skips attempts.
wait_s=$(( 120 * (1 << fail_n) ))
if [ "$wait_s" -gt 960 ]; then
  wait_s=960
fi
if [ "$fail_n" -gt 0 ] && [ $((now - last)) -lt "$wait_s" ]; then
  echo "backoff fail_n=$fail_n wait_s=$wait_s"
  exit 0
fi

if timeout 25 nmcli -w 20 connection up uuid "$UUID"; then
  rm -f "$STATE"
  echo "up uuid=$UUID"
  exit 0
fi
fail_n=$((fail_n + 1))
echo "$fail_n $now" >"$STATE"
echo "activate_failed fail_n=$fail_n"
exit 1
