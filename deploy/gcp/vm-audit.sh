#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh audit pipes it in): where the private key is, who can read it, what can reach the machine.
# Read-only, and the key itself is never shown: it is looked for as a pattern kept in a root-only file, deleted on exit,
# and only whether (and in which files) it is found is printed.
#
# For a test elsewhere: RT_ENV (the settings file), RT_STORE (the key kept encrypted), RT_CREDS (systemd-creds), RT_SEARCH (the
# folders searched) stand in for the VM's own, and RT_NO names tools to take as missing (systemctl, journalctl, …).
set -uo pipefail
F="${RT_ENV:-/etc/ruletaker/ruletaker.env}"
STORE="${RT_STORE:-/etc/credstore.encrypted/predict_private_key}"
CREDS="${RT_CREDS:-systemd-creds}"
SEARCH="${RT_SEARCH:-/etc /var/lib/ruletaker /opt/ruletaker /home /root /tmp /var/tmp /var/log}"
ok() { echo "✅ $*"; }
warn() { echo "⚠️ $*"; }
have() {
  case " ${RT_NO:-} " in *" $1 "*) return 1 ;; esac
  command -v "$1" >/dev/null 2>&1
}
kind_of() {  # what the kept key was sealed with (the first 16 bytes of systemd's format name it; as vm-key.sh reads it)
  case "$(head -c 24 "$1" 2>/dev/null | base64 -d 2>/dev/null | head -c 16 | od -An -tx1 | tr -d ' \n')" in
    93a894094874449090caf2fc93cab553|af4950a849134eb1a73846304ff30c05) echo "本机密钥 + 这台机器的虚拟 TPM：拷走磁盘也解不开" ;;
    0c7cc07b117645919c4b0bea08bc20fe|faf7eb9341e3412ca1a436f95a29362f) echo "这台机器的虚拟 TPM：拷走磁盘也解不开" ;;
    5a1c6a86df9d4096b1d5a65e0862f19a) echo "本机密钥：root 才读得到；这台机器的 TPM 没用上" ;;
    *) echo "systemd-creds" ;;
  esac
}

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
# PREDICT_PRIVATE_KEY line's (should there be more than one, commented out or not) and the one kept encrypted, each looked for.
P="$(mktemp -p /dev/shm 2>/dev/null || mktemp)"   # (in memory where there is a /dev/shm)
chmod 600 "$P"
trap 'shred -u "$P" 2>/dev/null || rm -f "$P"' EXIT
grep -E '^[[:space:]]*#*[[:space:]]*(export[[:space:]]+)?PREDICT_PRIVATE_KEY[[:space:]]*=' "$F" | grep -oiE '[0-9a-f]{64}' > "$P"
kept=""
if [ -e "$STORE" ]; then
  if have "$CREDS" && "$CREDS" decrypt --name=predict_private_key "$STORE" - 2>/dev/null | grep -oiE '[0-9a-f]{64}' >> "$P"; then
    kept=1
  fi
  smode="$(stat -c '%a %U:%G' "$STORE")"
  if [ -z "$kept" ]; then
    warn "加密保存的私钥 $STORE 在，但这台机器解不开它（换过虚拟机、或 TPM 重置过？）：重新设定 bash deploy/gcp/rt.sh key"
  elif [ "$smode" = "600 root:root" ]; then
    ok "私钥加密保存在 $STORE（$(kind_of "$STORE")；权限 $smode）"
  else
    warn "加密保存的私钥 $STORE 权限是 $smode（应为 600 root:root）"
  fi
fi
sort -u -o "$P" "$P"
pid=""
if have systemctl; then
  pid="$(systemctl show -p MainPID --value ruletaker 2>/dev/null || true)"
  [ "$pid" = "0" ] && pid=""
fi
if [ ! -s "$P" ]; then
  warn "没有私钥：设置文件里没有 PREDICT_PRIVATE_KEY，这台机器上也没有加密保存的（只能 off / paper；设定：bash deploy/gcp/rt.sh key）"
else
  if grep -qiF -f "$P" "$F"; then
    warn "设置文件里有明文私钥：运行 bash deploy/gcp/rt.sh update 改成加密保存（只有这台机器能解开），之后打开设置文件就看不到它"
  fi
  # every other file holding it in plain text (the settings file said above)
  # shellcheck disable=SC2086
  others="$(grep -rliF -f "$P" $SEARCH --exclude-dir=target --exclude-dir=.cargo --exclude-dir=venv --exclude-dir=journal 2>/dev/null \
    | grep -vxF "$P" | grep -vxF "$F" || true)"
  if [ -n "$others" ]; then
    warn "私钥的明文还出现在这些文件里（看完删掉：sudo shred -u 文件名）："
    printf '     %s\n' $others
  elif [ -n "$kept" ]; then
    grep -qiF -f "$P" "$F" || ok "除了加密保存的那份，哪个文件里都没有私钥的明文（设置文件里也没有；搜过：$SEARCH）"
  else
    ok "私钥只在 $F 这一个文件里（搜过：$SEARCH）"
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
      ok "私钥不在服务进程的环境变量和命令行里（服务自己读$([ -n "$kept" ] && echo " systemd 交给它的加密保存" || echo "设置文件")）"
    fi
    if [ -n "$kept" ]; then
      if [ -e /run/credentials/ruletaker.service/predict_private_key ]; then
        ok "服务从 systemd 拿到了解开的私钥：只在内存里（/run/credentials/ruletaker.service/），只有服务自己的用户能读，停了就没"
      else
        warn "服务没拿到加密保存的私钥（它的 systemd 配置是旧的？）：bash deploy/gcp/rt.sh update"
      fi
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
