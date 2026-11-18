#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh deploy / update call it): installs Python and the dependencies, runs the offline tests,
# puts the settings file in place (when one was uploaded) and (re)starts the service.
set -euo pipefail
APP=/opt/ruletaker
cd "$APP"

if ! id ruletaker >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/ruletaker --shell /usr/sbin/nologin ruletaker
fi
install -d -m 700 -o ruletaker -g ruletaker /var/lib/ruletaker
install -d -m 750 -o root -g ruletaker /etc/ruletaker

if ! dpkg -s python3-venv >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get -o DPkg::Lock::Timeout=300 update -qq   # (a new VM's first boot may hold the lock: unattended upgrades)
  apt-get -o DPkg::Lock::Timeout=300 install -y -qq python3 python3-venv python3-pip >/dev/null
fi
python3 -c 'import sys; assert sys.version_info >= (3, 11), sys.version' # (Ubuntu 24.04: 3.12, which also looks at a pushed book in the very turn it arrives)

if [ ! -x "$APP/venv/bin/python" ]; then
  python3 -m venv "$APP/venv"
fi
"$APP/venv/bin/pip" install -q --upgrade pip >/dev/null
"$APP/venv/bin/pip" install -q -r "$APP/requirements.txt"

echo "== 离线测试 =="
(cd "$APP" && "$APP/venv/bin/python" -m unittest discover -s tests -q)

if [ -f /tmp/ruletaker.env ]; then
  install -m 640 -o root -g ruletaker /tmp/ruletaker.env /etc/ruletaker/ruletaker.env
  shred -u /tmp/ruletaker.env 2>/dev/null || rm -f /tmp/ruletaker.env
  echo "== 设置文件已更新：/etc/ruletaker/ruletaker.env =="
fi
if [ ! -f /etc/ruletaker/ruletaker.env ]; then
  echo "!! 还没有设置文件 /etc/ruletaker/ruletaker.env：用 rt.sh env 上传后再启动" >&2
  exit 1
fi

echo "== 设置自检 =="
sudo -u ruletaker "$APP/venv/bin/python" -m ruletaker plan --env /etc/ruletaker/ruletaker.env   # (a setting that does not parse stops here)

install -m 644 "$APP/deploy/gcp/ruletaker.service" /etc/systemd/system/ruletaker.service
systemctl daemon-reload
systemctl enable ruletaker >/dev/null 2>&1
systemctl restart ruletaker
sleep 3
systemctl --no-pager --lines=15 status ruletaker || true
