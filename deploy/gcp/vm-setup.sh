#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh deploy / update / env call it): puts the uploaded settings file in place, installs Python and
# the dependencies (every package checked against the hashes in requirements.lock), runs the offline tests and (re)starts the
# service.
set -euo pipefail
APP=/opt/ruletaker
cd "$APP"

if ! id ruletaker >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/ruletaker --shell /usr/sbin/nologin ruletaker
fi
install -d -m 700 -o ruletaker -g ruletaker /var/lib/ruletaker
install -d -m 750 -o root -g ruletaker /etc/ruletaker

# The settings file first (it holds the private key): whatever fails further down, it is never left lying in /tmp.
if [ -f /tmp/ruletaker.env ]; then
  install -m 640 -o root -g ruletaker /tmp/ruletaker.env /etc/ruletaker/ruletaker.env
  shred -u /tmp/ruletaker.env 2>/dev/null || rm -f /tmp/ruletaker.env
  echo "== 设置文件已更新：/etc/ruletaker/ruletaker.env（root 所有，只有服务自己的用户组能读） =="
fi

if ! dpkg -s python3-venv >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get -o DPkg::Lock::Timeout=300 update -qq   # (a new VM's first boot may hold the lock: unattended upgrades)
  apt-get -o DPkg::Lock::Timeout=300 install -y -qq python3 python3-venv python3-pip >/dev/null
fi
# Python 3.12 or later: a pushed book is looked at, and its order sent, in the very turn of the loop it arrives in (eager tasks).
# Ubuntu 24.04 (the VM's image) has 3.12.
python3 -c 'import sys; assert sys.version_info >= (3, 12), "需要 Python 3.12 以上（Ubuntu 24.04 自带 3.12），现在是 " + sys.version'

if [ ! -x "$APP/venv/bin/python" ]; then
  python3 -m venv "$APP/venv"
fi
# Exactly the versions the tests ran with, each file checked against its hash (requirements.lock): a package swapped on the way,
# or a release published since, is refused. Ready-made wheels only — nothing is built (no build tool fetched unchecked).
"$APP/venv/bin/pip" install -q --require-hashes --only-binary :all: -r "$APP/requirements.lock"

echo "== 离线测试 =="
(cd "$APP" && "$APP/venv/bin/python" -m unittest discover -s tests -q)

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
