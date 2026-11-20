#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh deploy / update / env call it): puts the uploaded settings file in place, builds the service,
# keeps the private key encrypted instead of in the settings file (vm-key.sh) and (re)starts it.
#
#   RT_IMPL=rust (the default): the Rust version — Ubuntu's own Rust toolchain (rustc-1.xx / cargo-1.xx from the Ubuntu
#     archive, signed like every Ubuntu package), the crates exactly as Cargo.lock pins them (each checked against its sha256
#     by cargo), built once on the machine; its offline tests run before the service is touched.
#   RT_IMPL=python: the Python version (python/): its packages checked against the hashes in python/requirements.lock.
#
# Both keep their records in the same SQLite file (/var/lib/ruletaker/ruletaker.sqlite3) in the same format: switching between
# them keeps every record, the saved settings and the pause.
set -euo pipefail
APP=/opt/ruletaker
IMPL="${RT_IMPL:-$(cat "$APP/IMPL" 2>/dev/null || echo rust)}"   # (the one in use stays, unless RT_IMPL says otherwise)
[ "$IMPL" = rust ] || [ "$IMPL" = python ] || { echo "!! RT_IMPL 只能是 rust 或 python" >&2; exit 1; }
cd "$APP"

if ! id ruletaker >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/ruletaker --shell /usr/sbin/nologin ruletaker
fi
install -d -m 700 -o ruletaker -g ruletaker /var/lib/ruletaker
install -d -m 750 -o root -g ruletaker /etc/ruletaker
install -d -m 755 "$APP/bin"

# The settings file first (it holds the private key): whatever fails further down, it is never left lying in /tmp.
if [ -f /tmp/ruletaker.env ]; then
  install -m 640 -o root -g ruletaker /tmp/ruletaker.env /etc/ruletaker/ruletaker.env
  shred -u /tmp/ruletaker.env 2>/dev/null || rm -f /tmp/ruletaker.env
  echo "== 设置文件已更新：/etc/ruletaker/ruletaker.env（root 所有，只有服务自己的用户组能读） =="
fi

export DEBIAN_FRONTEND=noninteractive
apt_ready=""
apt_install() {  # apt_install PACKAGE... (the package lists read once a run; a new VM's first boot may hold the lock)
  if [ -z "$apt_ready" ]; then
    apt-get -o DPkg::Lock::Timeout=300 update -qq
    apt_ready=1
  fi
  apt-get -o DPkg::Lock::Timeout=300 install -y -qq "$@" >/dev/null
}

# The TPM's libraries, for systemd to seal the private key to this machine's virtual TPM as well (vm-key.sh): Ubuntu ships
# them as suggestions only. None to be had (another release's names): the key is sealed with the machine's own key alone.
if [ -e /dev/tpmrm0 ] && [ -z "$(ldconfig -p 2>/dev/null | grep 'libtss2-tcti-device\.so\.0' || true)" ]; then
  apt_install libtss2-esys-3.0.2-0t64 libtss2-mu-4.0.1-0t64 libtss2-rc0t64 libtss2-tcti-device0t64 2>/dev/null \
    || echo "（TPM 的库没装上：私钥只用本机密钥加密）"
fi

if [ "$IMPL" = "rust" ]; then
  # The newest of Ubuntu's own versioned Rust toolchains this release offers (Cargo.toml asks for 1.85 or later).
  CARGO=""
  for v in 1.97 1.91 1.89 1.85; do
    if [ -x "/usr/lib/rust-$v/bin/cargo" ]; then CARGO="/usr/bin/cargo-$v"; break; fi
  done
  if [ -z "$CARGO" ]; then
    apt-get -o DPkg::Lock::Timeout=300 update -qq
    apt_ready=1
    for v in 1.97 1.91 1.89 1.85; do
      if apt-cache show "cargo-$v" >/dev/null 2>&1; then
        echo "== 安装 Ubuntu 官方源的 Rust 工具链 $v（第一次要几分钟） =="
        apt_install "rustc-$v" "cargo-$v" build-essential pkg-config ca-certificates
        CARGO="/usr/bin/cargo-$v"
        break
      fi
    done
  fi
  [ -n "$CARGO" ] || { echo "!! Ubuntu 源里没有 1.85 以上的 Rust 工具链（rustc-1.xx）：换 Ubuntu 24.04 镜像，或用 RT_IMPL=python" >&2; exit 1; }
  dpkg -s build-essential >/dev/null 2>&1 || apt_install build-essential pkg-config ca-certificates
  export CARGO_HOME="$APP/.cargo" CARGO_TARGET_DIR="$APP/target"
  echo "== 编译（$($CARGO --version)；依赖按 Cargo.lock 锁定的版本和哈希；第一次约 3～5 分钟，之后只编译改动） =="
  "$CARGO" build --release --locked --quiet
  echo "== 离线测试 =="
  if ! out="$("$CARGO" test --release --locked -q 2>&1)"; then
    echo "$out" | tail -60
    echo "!! 测试没通过：服务没有动（还是原来的版本在运行）" >&2
    exit 1
  fi
  echo "$out" | grep -E "test result" | tail -1
  install -m 755 "$APP/target/release/ruletaker" "$APP/bin/ruletaker.new"
  mv -f "$APP/bin/ruletaker.new" "$APP/bin/ruletaker"
  rm -f /etc/systemd/system/ruletaker.service.d/python.conf
else
  dpkg -s python3-venv >/dev/null 2>&1 || apt_install python3 python3-venv python3-pip
  # Python 3.12 or later: a pushed book is looked at, and its order sent, in the very turn of the loop it arrives in.
  python3 -c 'import sys; assert sys.version_info >= (3, 12), "需要 Python 3.12 以上（Ubuntu 24.04 自带 3.12），现在是 " + sys.version'
  [ -x "$APP/python/venv/bin/python" ] || python3 -m venv "$APP/python/venv"
  # Exactly the versions the tests ran with, each file checked against its hash; ready-made wheels only.
  "$APP/python/venv/bin/pip" install -q --require-hashes --only-binary :all: -r "$APP/python/requirements.lock"
  echo "== 离线测试 =="
  (cd "$APP/python" && "$APP/python/venv/bin/python" -m unittest discover -s tests -q)
  install -d -m 755 /etc/systemd/system/ruletaker.service.d
  install -m 644 "$APP/deploy/gcp/ruletaker-python.conf" /etc/systemd/system/ruletaker.service.d/python.conf
fi
echo "$IMPL" > "$APP/IMPL"

if [ ! -f /etc/ruletaker/ruletaker.env ]; then
  echo "!! 还没有设置文件 /etc/ruletaker/ruletaker.env：用 rt.sh env 上传后再启动" >&2
  exit 1
fi

# The unit first (it opens a key kept encrypted), then the key out of the settings file into encrypted keeping — proved to
# open before anything is changed; anything wrong leaves it where it was — then the settings checked the way the service
# will read them (its user, its key), and only then the restart.
install -m 644 "$APP/deploy/gcp/ruletaker.service" /etc/systemd/system/ruletaker.service
systemctl daemon-reload
systemctl enable ruletaker >/dev/null 2>&1
bash "$APP/deploy/gcp/vm-key.sh" move || echo "（私钥照旧在设置文件里，服务照常运行；原因见上面）"
bash "$APP/deploy/gcp/vm-key.sh" status
echo "== 设置自检 =="
bash "$APP/deploy/gcp/vm-key.sh" run plan --env /etc/ruletaker/ruletaker.env   # (a setting that does not parse stops here)
systemctl restart ruletaker
sleep 3
systemctl --no-pager --lines=15 status ruletaker || true
echo "== 现在运行的是：$([ "$IMPL" = rust ] && echo "Rust 版（$APP/bin/ruletaker）" || echo "Python 版（$APP/python）") =="
