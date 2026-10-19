# Isolation (live scheme: iptables-legacy)

Confirm before applying. Do not `iptables -F` / `nft flush ruleset`. Keep host SSH, AP DNS,
and any existing tunnels. Do not create a public Cloudflare Tunnel in this round.

Verifier is the already-authorized account `user` (uid 1000) using `sudo` or ADB root.
There is no `maint` account. Commands below must not be run as `aegis` and must not
assume `maint` exists.

**nft is retired on this stick.** `nft` CLI was absent; `iptables` alternatives point at
`iptables-legacy`. Do not run old `nft add table inet aegis-iso` snippets.
See `deploy/nft_iso_retired.sh` (exits 3). Deploy / persist / rollback:

- `deploy/apply_iso_legacy.sh`
- `deploy/persist_iso.sh` → `/etc/iptables/rules.v4` and `rules.v6`
- `deploy/rollback_iso.sh` (deletes only owner/8787 rules; does not flush)

## Goal

- `cloudflared` (future, separate uid) may dial `127.0.0.1:8787` and complete that TCP exchange.
- `aegis` may accept that inbound loopback connection and send return packets for *established* sessions.
- `aegis` must not start NEW connections to loopback, USB (`usb0`), Wi-Fi, other LAN, or IPv6.
- systemd `IPAddressAllow=127.0.0.1/32` still permits `connect()` to loopback; OUTPUT owner DROP must drop those NEW sockets. Do not treat systemd as outbound deny.

## iptables-legacy (own rules only, no flush)

```
iptables-legacy -A OUTPUT -m owner --uid-owner aegis -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
iptables-legacy -A OUTPUT -m owner --uid-owner aegis -j DROP
iptables-legacy -A INPUT -i lo -p tcp --dport 8787 -j ACCEPT
iptables-legacy -A INPUT -p tcp --dport 8787 -j DROP
```

Same four with `ip6tables-legacy`. Use the **name** `aegis`, not a hardcoded uid.

## Identity in proofs

`nsenter -t PID -m` enters the mount namespace and keeps the caller uid (often 0).
That is valid for checking InaccessiblePaths / read-only mounts.
It is **not** an aegis DAC read test.

`nsenter -S/-G` sets uid/gid but does not `initgroups` (see nsenter(1)).
Always print `id -u/-g/-G`, `/proc/self/status` Uid/Gid/Groups/CapEff.

Aegis file-read proofs must use `runuser -u aegis` (full identity) **and** print that identity.

## Rollback

```
sh deploy/rollback_iso.sh
# then persist if the cleared rules should survive reboot
# sh deploy/persist_iso.sh
sudo systemctl stop aegis
```

Existing host SSH on the USB gadget interface must keep working.
