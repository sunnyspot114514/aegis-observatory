# Wi-Fi unattended recovery

1033 has two distinct causes. Do not restart the tunnel for both.

| Case | What you see | What not to do |
|------|----------------|----------------|
| `wlan0` down, no default route | USB only, ping WAN fails | Do not bounce cloudflared |
| WAN up, IPv4 TCP 7844 blocked, IPv6 may work | ICMP/HTTPS ok, tunnel 1033 | Do not force `edge-ip-version=6` if there is no IPv6 default route; do not flap Wi-Fi |

USB gadget SSH stays. Do not auto-switch to a phone hotspot. Do not touch AgentHub. Do not change the home LAN.

## What a headless handshake drop looks like

The configured AP can drop during 4-way handshake (`supplicant-disconnect`). NetworkManager then asks for a **new key**. On a stick with no GUI secrets agent, that logs `no-secrets` and **fails activation**. Autoconnect may stop after the default retry count (typically 4 when `autoconnect-retries=-1`). A later `nmcli connection up` can still succeed because the **stored PSK was still there**. `no-secrets` ≠ missing password.

## Inspect (no secrets)

```
nmcli -f connection.id,connection.uuid,connection.autoconnect,connection.autoconnect-retries,connection.permissions,802-11-wireless.ssid,802-11-wireless.cloned-mac-address,802-11-wireless.mac-address-randomization,802-11-wireless-security.key-mgmt,802-11-wireless-security.psk-flags connection show YOUR_SSID
```

Confirm the connection UUID is unique. Do not `show-secrets`, do not log PSK, do not commit a connection file that contains credentials.

On this project's operator instance, `connection.autoconnect-retries` was changed from `-1` (default ≈ 4) to `0` (forever in NetworkManager 1.42). MAC cloning / randomization was **not** changed. The optional `aegis-wifi-watch.timer` is **not** installed.

**Rollback of that retries change:** `nmcli connection modify uuid YOUR_NM_CONNECTION_UUID connection.autoconnect-retries -1`

## Optional timer (not installed)

`aegis-wifi-watch.timer` every 2 minutes: if the target UUID is down **and** not already activating, `nmcli connection up uuid …`. Timeouts, flock mutex, backoff on failure. If Wi-Fi is connected, **do not** disconnect it to “fix” Cloudflare. Do not install until confirmed.
