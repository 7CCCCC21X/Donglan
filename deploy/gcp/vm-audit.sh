#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh audit pipes it in): where the private key is, who can read it, what can reach the machine.
# Read-only, and the key itself is never shown: it is looked for as a pattern kept in a root-only file, deleted on exit,
# and only whether (and in which files) it is found is printed.
#
# For a test elsewhere: RT_ENV (the settings file) and RT_SEARCH (the folders searched) stand in for the VM's own.
set -uo pipefail
F="${RT_ENV:-/etc/ruletaker/ruletaker.env}"
SEARCH="${RT_SEARCH:-/etc /var/lib/ruletaker /opt/ruletaker /home /root /tmp /var/tmp /var/log}"
ok() { echo "✅ $*"; }
warn() { echo "⚠️ $*"; }
have() { command -v "$1" >/dev/null 2>&1; }

echo "-- 虚拟机上 --"
if [ ! -f "$F" ]; then
  warn "没有设置文件 $F"
  exit 0
fi
mode="$(stat -c '%a %U:%G' "$F")"
if [ "$mode" = "640 root:ruletaker" ]; then
  ok "设置文件 $F：权限 $mode（只有 root 和服务自己的用户组能读）"
else
  warn "设置文件 $F 的权限是 $mode（应为 640 root:ruletaker；改正：sudo chown root:ruletaker $F && sudo chmod 640 $F）"
fi
dir="$(dirname "$F")"
dmode="$(stat -c '%a %U:%G' "$dir")"
if [ "$dmode" = "750 root:ruletaker" ]; then
  ok "目录 $dir：$dmode"
else
  warn "目录 $dir 是 $dmode（应为 750 root:ruletaker）"
fi
# a name written twice: the service takes its first line (a key "changed" by adding a line below the old one is not changed)
dups="$(grep -E '^[[:space:]]*(export[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=' "$F" \
  | sed -E 's/^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*).*/\2/' | sort | uniq -d | paste -sd ' ' -)"
if [ -n "$dups" ]; then
  warn "设置文件里这些名字写了不止一行，服务只认第一行：${dups}（bash deploy/gcp/rt.sh edit 删掉多余的）"
else
  ok "设置文件里没有重复写的名字"
fi
if [ -e /tmp/ruletaker.env ]; then
  warn "/tmp/ruletaker.env 还在（上传后没收进 /etc）：sudo shred -u /tmp/ruletaker.env"
else
  ok "/tmp 里没有残留的设置文件"
fi

# The key as a fixed-string pattern (its 64 hex digits, with or without 0x), in a file only root reads, gone on exit: every
# PREDICT_PRIVATE_KEY line's (should there be more than one), each looked for.
P="$(mktemp)"
chmod 600 "$P"
trap 'shred -u "$P" 2>/dev/null || rm -f "$P"' EXIT
grep -E '^[[:space:]]*(export[[:space:]]+)?PREDICT_PRIVATE_KEY[[:space:]]*=' "$F" | grep -oiE '[0-9a-f]{64}' | sort -u > "$P"
pid=""
if have systemctl; then
  pid="$(systemctl show -p MainPID --value ruletaker 2>/dev/null || true)"
  [ "$pid" = "0" ] && pid=""
fi
if [ ! -s "$P" ]; then
  warn "设置文件里没有私钥（PREDICT_PRIVATE_KEY 是空的）"
else
  # shellcheck disable=SC2086
  where="$(grep -rliF -f "$P" $SEARCH --exclude-dir=target --exclude-dir=.cargo --exclude-dir=venv --exclude-dir=journal 2>/dev/null \
    | grep -vxF "$F" | grep -vxF "$P" || true)"
  if [ -z "$where" ]; then
    ok "私钥只在 $F 这一个文件里（搜过：$SEARCH）"
  else
    warn "私钥还出现在这些文件里（看完删掉：sudo shred -u 文件名）："
    printf '     %s\n' $where
  fi
  if have journalctl; then
    n="$(journalctl --no-pager -q 2>/dev/null | grep -ciF -f "$P" || true)"
    if [ "${n:-0}" = "0" ]; then ok "系统日志里没有私钥"; else warn "系统日志里有 ${n} 行带着私钥"; fi
  fi
  if [ -n "$pid" ]; then
    if tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -qiF -f "$P"; then
      warn "私钥在服务进程的环境变量里"
    elif tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qiF -f "$P"; then
      warn "私钥在服务进程的命令行参数里（ps 能看到）"
    else
      ok "私钥不在服务进程的环境变量和命令行里（服务自己读设置文件）"
    fi
  fi
fi

if [ -n "$pid" ]; then
  user="$(ps -o user= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ "$user" = "ruletaker" ]; then ok "服务以专用用户 ruletaker 运行（不是 root，不能登录）"; else warn "服务的运行用户是 ${user:-?}（应为 ruletaker）"; fi
  core="$(awk '/core file size/ {print $5}' "/proc/$pid/limits" 2>/dev/null)"
  if [ "$core" = "0" ]; then ok "服务崩溃时不写 core dump（内存里的私钥不会落盘）"; else warn "core dump 上限是 ${core:-?}（应为 0）"; fi
  if have ss; then
    listen="$(ss -Hltnp 2>/dev/null | grep "pid=$pid," | awk '{print $4}' | sort -u | tr '\n' ' ')"
    open="$(printf '%s\n' $listen | grep -vE '^(127\.0\.0\.1|\[::1\]|localhost):' || true)"
    if [ -z "$open" ]; then ok "服务只在本机监听（${listen:-没有端口}），公网连不到控制台"; else warn "服务在这些地址监听：$open"; fi
  fi
else
  warn "服务没在运行（systemctl status ruletaker 看原因）"
fi
if have swapon; then
  if [ -z "$(swapon --show --noheadings 2>/dev/null)" ]; then ok "没有交换分区（内存里的私钥不会被换到磁盘）"; else warn "开着交换分区：内存可能被写到磁盘"; fi
fi
if have sshd; then
  pw="$(sshd -T 2>/dev/null | awk '/^passwordauthentication/ {print $2}')"
  if [ "$pw" = "no" ]; then ok "SSH 只认密钥，不认密码"; else warn "SSH 允许密码登录（${pw:-?}）"; fi
fi
if have systemctl; then
  if systemctl is-enabled unattended-upgrades >/dev/null 2>&1; then ok "自动安装安全更新：开着"; else warn "自动安全更新没开"; fi
fi
admins="$(getent group google-sudoers 2>/dev/null | cut -d: -f4)"
[ -n "$admins" ] || admins="$(getent group sudo 2>/dev/null | cut -d: -f4)"
echo "能用 sudo 的账号（它们都读得到私钥，应该只有你自己）：${admins:-（没有）}"
