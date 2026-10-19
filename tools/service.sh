#!/bin/sh
# Runs efema-server as a systemd service on this Linux machine - or moves a
# running one onto the binary installed since. As root, after install.sh:
#
#   curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sudo EFEMA_INSTALL_DIR=/usr/local/bin sh
#   curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/service.sh | sudo sh
#
# Makes the `efema` system user and /var/lib/efema the first time, writes the
# unit, and starts the relay. A relay that was running is stopped first and
# its data copied aside to /var/lib/efema.backups - a newer binary may migrate
# the database, and a migration only goes forward; the newest five copies are
# kept. Then it waits for the relay to answer /health and says which version
# does.
#
# The unit is this script's and is written anew every time. Settings are
# yours: EFEMA_* lines in /etc/efema/serve.env (EFEMA_LISTEN, EFEMA_RATE,
# EFEMA_TRUST_PROXY...), which this script never touches.
set -eu

BIN=/usr/local/bin/efema-server
DATA=/var/lib/efema
BACKUPS=/var/lib/efema.backups
KEEP=5
SETTINGS=/etc/efema/serve.env
UNIT=/etc/systemd/system/efema-server.service

if [ "$(id -u)" != 0 ]; then
    echo "Run it as root: sudo sh service.sh" >&2
    exit 1
fi
if ! command -v systemctl >/dev/null 2>&1; then
    echo "This sets efema-server up as a systemd service, and this machine has no systemd." >&2
    exit 1
fi
if [ ! -x "$BIN" ]; then
    echo "No efema-server at $BIN - install it there first:" >&2
    echo "  curl -fsSL https://raw.githubusercontent.com/lacodda/efema/main/tools/install.sh | sudo EFEMA_INSTALL_DIR=/usr/local/bin sh" >&2
    exit 1
fi

if ! id efema >/dev/null 2>&1; then
    useradd --system --home-dir "$DATA" --no-create-home --shell /usr/sbin/nologin efema
    echo "Made the system user efema"
fi
install -d -o efema -g efema -m 750 "$DATA"
install -d -m 755 /etc/efema

if systemctl is-active --quiet efema-server && [ -e "$DATA/relay.sqlite3" ]; then
    systemctl stop efema-server
    # Stopped, so the copy is whole: no commit is caught halfway.
    stamp=$(date -u +%Y%m%dT%H%M%SZ)
    install -d -m 700 "$BACKUPS"
    cp -a "$DATA" "$BACKUPS/$stamp"
    echo "Copied $DATA to $BACKUPS/$stamp"
    ls -1 "$BACKUPS" | sort -r | tail -n +$((KEEP + 1)) | while read -r old; do
        rm -rf "${BACKUPS:?}/$old"
    done
fi

cat >"$UNIT" <<'UNIT'
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
UNIT

systemctl daemon-reload
systemctl enable --quiet efema-server
systemctl restart efema-server

# Asked on loopback, at the port the relay listens on: an address on every
# interface (0.0.0.0, [::]) answers there too.
listen=$(sed -n 's/^EFEMA_LISTEN=//p' "$SETTINGS" 2>/dev/null | tail -n 1)
port=${listen##*:}
port=${port:-2451}
for _ in $(seq 1 30); do
    if curl -fsS --max-time 2 "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
        echo "$("$BIN" --version) answers on port $port, data in $DATA"
        exit 0
    fi
    sleep 1
done
echo "efema-server did not answer http://127.0.0.1:$port/health within 30 seconds:" >&2
journalctl -u efema-server -n 20 --no-pager >&2 || true
exit 1
