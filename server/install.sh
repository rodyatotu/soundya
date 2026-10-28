#!/bin/sh
# Installs the soundya statistics server on a Linux VPS (Debian/Ubuntu and alike):
#   scp -r server root@<vps>:/root/soundya-stats && ssh root@<vps> sh /root/soundya-stats/install.sh
# Needs python3 (any 3.8+). Listens on port 8787; data in /var/lib/soundya-stats.
set -eu
PORT=${PORT:-8787}
SRC=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
command -v python3 >/dev/null || { apt-get update && apt-get install -y python3; }
id soundya-stats >/dev/null 2>&1 || useradd --system --home /var/lib/soundya-stats --shell /usr/sbin/nologin soundya-stats
install -d -o soundya-stats -g soundya-stats /var/lib/soundya-stats
install -D -m 755 "$SRC/stats.py" /opt/soundya-stats/stats.py
cat > /etc/systemd/system/soundya-stats.service <<UNIT
[Unit]
Description=soundya statistics (counts running speakers)
After=network-online.target

[Service]
User=soundya-stats
ExecStart=/usr/bin/python3 /opt/soundya-stats/stats.py --port $PORT --db /var/lib/soundya-stats/stats.db
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/soundya-stats

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable --now soundya-stats
systemctl restart soundya-stats
# Open the port if a firewall is in use.
if command -v ufw >/dev/null && ufw status | grep -q active; then ufw allow "$PORT/tcp"; fi
sleep 1
IP=$(curl -s4 --max-time 5 https://ifconfig.me 2>/dev/null || hostname -I | cut -d' ' -f1)
echo
curl -s "http://127.0.0.1:$PORT/badge.json" && echo
echo "Готово. Значок: http://$IP:$PORT/badge.svg  ·  статистика: http://$IP:$PORT/"
