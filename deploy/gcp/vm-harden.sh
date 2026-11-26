#!/usr/bin/env bash
# Runs ON the VM as root (vm-setup.sh, on every deploy / update / env): the machine itself made harder to get into and to look
# into — kernel settings that keep its memory, its pointers and its logs from other users and its network from redirect and
# spoofing tricks; SSH taking keys alone, never root, with nothing forwarded but the console's own local port. Every file here
# is this script's own (none of Ubuntu's or Google's is edited), checked before it is used, and taken out again should the
# check fail: nothing here can lock anyone out. Run it again: it writes the same files and changes nothing more.
#
#   bash vm-harden.sh          apply (what was done, line by line)
#   bash vm-harden.sh status   what is in force (read-only)
#
# For a test elsewhere: RT_SYSCTL_D, RT_SSHD_D (the folders), RT_SYSCTL, RT_SSHD, RT_SYSTEMCTL (the tools) stand in for the VM's.
set -uo pipefail
SYSCTL_D="${RT_SYSCTL_D:-/etc/sysctl.d}"
SSHD_D="${RT_SSHD_D:-/etc/ssh/sshd_config.d}"
SYSCTL="${RT_SYSCTL:-sysctl}"
SSHD="${RT_SSHD:-sshd}"
SYSTEMCTL="${RT_SYSTEMCTL:-systemctl}"
KERNEL="$SYSCTL_D/90-ruletaker.conf"
# (first of the folder's files: sshd takes the first value it reads of a setting, so these win over any later file's)
SSH_CONF="$SSHD_D/01-ruletaker.conf"

kernel_conf() {
  cat <<'CONF'
# 规则吃单的服务器加固（deploy/gcp/vm-harden.sh 写的，每次 update 重写；删掉它并重启就恢复 Ubuntu 默认）
# 内核：别的用户看不到内核地址和内核日志，进程之间不能互相读内存（ptrace，只有 root 可以），setuid 程序不写 core dump
kernel.kptr_restrict = 2
kernel.dmesg_restrict = 1
kernel.yama.ptrace_scope = 2
kernel.unprivileged_bpf_disabled = 2
fs.suid_dumpable = 0
fs.protected_fifos = 2
fs.protected_regular = 2
# 网络：不接受 ICMP 重定向和源路由（别人改不了这台机器的路由），自己也不发重定向，记下来源不对的包，SYN 洪水用 cookie 扛
net.ipv4.conf.all.accept_redirects = 0
net.ipv4.conf.default.accept_redirects = 0
net.ipv4.conf.all.secure_redirects = 0
net.ipv4.conf.default.secure_redirects = 0
net.ipv4.conf.all.send_redirects = 0
net.ipv4.conf.default.send_redirects = 0
net.ipv4.conf.all.accept_source_route = 0
net.ipv4.conf.default.accept_source_route = 0
net.ipv6.conf.all.accept_redirects = 0
net.ipv6.conf.default.accept_redirects = 0
net.ipv6.conf.all.accept_source_route = 0
net.ipv4.conf.all.log_martians = 1
net.ipv4.icmp_echo_ignore_broadcasts = 1
net.ipv4.icmp_ignore_bogus_error_responses = 1
net.ipv4.tcp_syncookies = 1
CONF
}

ssh_conf() {
  cat <<'CONF'
# 规则吃单的 SSH 加固（deploy/gcp/vm-harden.sh 写的，每次 update 重写）：只认密钥（gcloud / IAP 登录用的就是密钥），root 不能
# 登录；不转发图形界面、登录代理和网络隧道，只留本地端口转发（rt.sh console 打开网页控制台用它）；登录 30 秒内没完成就断开
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitEmptyPasswords no
PermitRootLogin no
X11Forwarding no
AllowAgentForwarding no
AllowTcpForwarding local
PermitTunnel no
GatewayPorts no
LoginGraceTime 30
CONF
}

put() {  # put FILE GENERATOR: FILE written from GENERATOR's text (in place, by a rename) unless it already says exactly that
  local file="$1" gen="$2" tmp
  tmp="$(mktemp "$file.XXXXXX")" || return 1
  "$gen" > "$tmp" && chmod 644 "$tmp" || { rm -f "$tmp"; return 1; }
  if cmp -s "$tmp" "$file"; then rm -f "$tmp"; return 2; fi
  mv -f "$tmp" "$file"
}

case "${1:-apply}" in
  apply)
    install -d -m 755 "$SYSCTL_D"
    put "$KERNEL" kernel_conf
    case $? in
      0|2)
        # (-e: a setting this kernel does not have is passed over, never an error)
        if "$SYSCTL" -q -e -p "$KERNEL" >/dev/null 2>&1; then
          echo "✅ 内核加固：别的用户看不到内核地址和日志、进程间不能互相读内存、不接受 ICMP 重定向和源路由（$KERNEL）"
        else
          echo "⚠️ 内核加固的设置没全部生效（$KERNEL；重启后生效）"
        fi
        ;;
      *) echo "⚠️ 内核加固没写成（$KERNEL）：照旧" ;;
    esac
    if [ -d "$SSHD_D" ] || install -d -m 755 "$SSHD_D"; then
      put "$SSH_CONF" ssh_conf
      written=$?
      install -d -m 755 /run/sshd 2>/dev/null || true   # (sshd -t wants it; ssh.service makes it on its own start)
      if [ "$written" = 1 ]; then
        echo "⚠️ SSH 加固没写成（$SSH_CONF）：SSH 照旧"
      elif err="$("$SSHD" -t 2>&1)"; then
        [ "$written" = 0 ] && { "$SYSTEMCTL" reload ssh >/dev/null 2>&1 || "$SYSTEMCTL" reload sshd >/dev/null 2>&1 || true; }
        echo "✅ SSH 加固：只认密钥、root 不能登录、只留本地端口转发（$SSH_CONF）"
      else
        # (a check that fails leaves SSH without this file — whatever failed it: never a configuration sshd would not take)
        rm -f "$SSH_CONF"
        if "$SSHD" -t >/dev/null 2>&1; then
          echo "⚠️ SSH 加固没用上：这台机器的 sshd 不认它（$(printf '%s' "$err" | tail -1)），已拿掉，SSH 照旧"
        else
          echo "⚠️ SSH 加固没用上：sshd 的配置本来就检查不过（$(printf '%s' "$err" | tail -1)），这个文件没放进去"
        fi
      fi
    fi
    ;;
  status)
    if [ -f "$KERNEL" ]; then echo "内核加固：$KERNEL"; else echo "内核加固：没有"; fi
    if [ -f "$SSH_CONF" ]; then echo "SSH 加固：$SSH_CONF"; else echo "SSH 加固：没有"; fi
    ;;
  *)
    echo "用法：bash vm-harden.sh [apply | status]"
    exit 2
    ;;
esac
