---
title: On a server
description: Run the relay on a Linux server as a systemd service, behind an HTTPS proxy - install it, hand out keys, keep it upgraded.
---

On a Linux server the relay runs as a systemd service, as a user of its own,
with its database in `/var/lib/efema`. It listens on loopback; a reverse proxy
on the same machine takes HTTPS and passes requests on. Two commands set it
up, and the same two upgrade it.

## Install

```console
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sudo EFEMA_INSTALL_DIR=/usr/local/bin sh
Downloading https://github.com/lacodda/efema/releases/download/v0.9.0/efema-server-v0.9.0-aarch64-unknown-linux-gnu.tar.gz
Installed efema-server 0.9.0 to /usr/local/bin/efema-server
Next: https://lacodda.github.io/efema/getting-started/ - or, as a service on a Linux server, https://lacodda.github.io/efema/guides/on-a-server/
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/service.sh | sudo sh
Made the system user efema
efema-server 0.9.0 answers on port 2451, data in /var/lib/efema
```

The first puts the binary from the latest release - for x86_64 or 64-bit ARM,
a Raspberry Pi among them - in `/usr/local/bin`; `EFEMA_VERSION=v0.9.0` pins a
release instead. The second, which needs systemd:

- makes the system user `efema` and its data directory, the first time;
- writes the unit below and starts the relay, and starts it again at boot;
- waits until the relay answers `/health`, and says which version does.

```ini
[Unit]
Description=efema relay: keeps sealed changes in order for the devices that sync through it
Documentation=https://lacodda.github.io/efema/guides/on-a-server/
After=network-online.target
Wants=network-online.target

[Service]
User=efema
Group=efema
Environment=EFEMA_DATA=/var/lib/efema
# Your settings, as EFEMA_* lines: this unit is written anew on every
# upgrade, that file never is.
EnvironmentFile=-/etc/efema/serve.env
ExecStart=/usr/local/bin/efema-server serve
Restart=on-failure
RestartSec=2
# The relay needs its data directory and the network - nothing else of the
# machine.
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ReadWritePaths=/var/lib/efema

[Install]
WantedBy=multi-user.target
```

## Settings

Every option of [`serve`](/efema/reference/serve/) has an `EFEMA_*` name.
Put the ones you change in `/etc/efema/serve.env`, then restart:

```sh
# /etc/efema/serve.env
EFEMA_SPACE_QUOTA=2GiB
EFEMA_RATE=300
```

```console
$ sudo systemctl restart efema-server
```

The unit is the script's and is written anew on every upgrade; this file is
yours and is never touched.

## Hand out keys

Keys are written to the relay's database, so hand them out as its user:

```console
$ sudo -u efema efema-server keys add home write --label laptop --data /var/lib/efema
```

The key is printed once, on standard output. [`keys`](/efema/reference/keys/)
lists and revokes them, while the relay runs.

## The door

The relay answers only its own machine and HTTPS through a proxy. With Caddy
on the same server:

```text title="Caddyfile"
sync.example.com {
	reverse_proxy 127.0.0.1:2451
}
```

Any other proxy, a proxy on another host, and what each needs:
[Behind a reverse proxy](/efema/guides/behind-a-reverse-proxy/).

## Check it

From another device:

```console
$ curl https://sync.example.com/health
ok
$ curl -s -o /dev/null -w '%{http_code}\n' https://sync.example.com/v1/limits
401
$ curl -s -o /dev/null -w '%{http_code}\n' https://sync.example.com/metrics
403
```

The door is open; `/v1` wants a key; the metrics answer only the server
itself. On the server, `journalctl -u efema-server` has the relay's log -
what it wrote and read, never what an entry holds.

## Upgrade

The same two commands. Before the relay starts on the new binary, the script
stops it and copies its data directory aside - a newer release may move the
database to a newer schema, and that only goes forward:

```console
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sudo EFEMA_INSTALL_DIR=/usr/local/bin sh
Downloading https://github.com/lacodda/efema/releases/download/v0.9.0/efema-server-v0.9.0-aarch64-unknown-linux-gnu.tar.gz
Installed efema-server 0.9.0 to /usr/local/bin/efema-server
Next: https://lacodda.github.io/efema/getting-started/ - or, as a service on a Linux server, https://lacodda.github.io/efema/guides/on-a-server/
$ curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/service.sh | sudo sh
Copied /var/lib/efema to /var/lib/efema.backups/20261031T201007Z
efema-server 0.9.0 answers on port 2451, data in /var/lib/efema
```

The newest five copies are kept in `/var/lib/efema.backups`. Putting one back
is the same as any [backup](/efema/guides/backups/#restoring).

## Related

- [Getting started](/efema/getting-started/) - the relay on your own machine.
- [Behind a reverse proxy](/efema/guides/behind-a-reverse-proxy/) - the door.
- [Backups](/efema/guides/backups/) - taking them while it runs, and putting
  one back.
- [When something goes wrong](/efema/guides/when-something-goes-wrong/) - a
  full space, a key to replace, a move to another machine.
