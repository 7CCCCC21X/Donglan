#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh lockdown / unlock call it): this machine's own firewall (ruletaker-firewall.nft), the second
# layer under Google Cloud's — put on with its undo already set (UNDO_S seconds later the table is deleted again unless
# `confirm` comes first, so the door is proved to open to a fresh IAP login before it is kept), loaded at every boot once
# confirmed, taken off again. Nobody is ever locked out: whatever goes wrong after `arm`, the undo runs on its own.
#
#   bash vm-firewall.sh arm       the ruleset checked, the undo set, then the ruleset loaded (an open SSH session stays)
#   bash vm-firewall.sh confirm   the undo called off, the ruleset loaded at every boot
#   bash vm-firewall.sh remove    the ruleset unloaded, and no longer loaded at boot
#   bash vm-firewall.sh status    on | pending (armed, not confirmed) | off — read-only
#
# For a test elsewhere: RT_NFT, RT_SYSTEMCTL, RT_SYSTEMD_RUN (the tools), RT_FW_DIR (/etc/ruletaker), RT_UNIT_DIR
# (/etc/systemd/system), RT_SRC (the deploy folder), RT_UNDO_S (the undo's seconds) stand in for the VM's own.
set -uo pipefail
NFT="${RT_NFT:-nft}"
SYSTEMCTL="${RT_SYSTEMCTL:-systemctl}"
SDRUN="${RT_SYSTEMD_RUN:-systemd-run}"
FW_DIR="${RT_FW_DIR:-/etc/ruletaker}"
UNIT_DIR="${RT_UNIT_DIR:-/etc/systemd/system}"
SRC="${RT_SRC:-/opt/ruletaker/deploy/gcp}"
UNDO_S="${RT_UNDO_S:-180}"
RULES="$FW_DIR/firewall.nft"
UNIT="$UNIT_DIR/ruletaker-firewall.service"
UNDO=ruletaker-firewall-undo
TABLE=(inet ruletaker_guard)

loaded() { "$NFT" list table "${TABLE[@]}" >/dev/null 2>&1; }

case "${1:-}" in
  arm)
    if ! command -v "$NFT" >/dev/null 2>&1; then
      DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=120 install -y -qq nftables >/dev/null 2>&1 \
        || { echo "❌ 装不上 nftables：机器防火墙没加（Google Cloud 的防火墙照旧）"; exit 1; }
    fi
    install -d -m 750 "$FW_DIR"
    install -m 600 "$SRC/ruletaker-firewall.nft" "$RULES.new" || { echo "❌ 没有防火墙规则文件：什么都没改"; exit 1; }
    if ! err="$("$NFT" -c -f "$RULES.new" 2>&1)"; then
      rm -f "$RULES.new"
      echo "❌ 防火墙规则这台机器不认（$(printf '%s' "$err" | tail -1)）：什么都没改"
      exit 1
    fi
    mv -f "$RULES.new" "$RULES"
    # the undo first: whatever becomes of this session, the table goes again on its own unless confirmed
    "$SYSTEMCTL" stop "$UNDO.timer" >/dev/null 2>&1 || true
    "$SYSTEMCTL" reset-failed "$UNDO.timer" "$UNDO.service" >/dev/null 2>&1 || true
    if ! "$SDRUN" --quiet --unit="$UNDO" --on-active="$UNDO_S" "$(command -v "$NFT")" delete table "${TABLE[@]}"; then
      echo "❌ 设不了自动撤销：机器防火墙没加（不冒被锁在外面的险）"
      exit 1
    fi
    if ! "$NFT" -f "$RULES"; then
      "$SYSTEMCTL" stop "$UNDO.timer" >/dev/null 2>&1 || true
      echo "❌ 防火墙规则没加上：什么都没改"
      exit 1
    fi
    echo "机器防火墙已加上（${UNDO_S} 秒内不确认就自动撤销）"
    ;;
  confirm)
    loaded || { echo "❌ 机器防火墙不在（自动撤销已经跑了？）：再运行一次 bash deploy/gcp/rt.sh lockdown"; exit 1; }
    "$SYSTEMCTL" stop "$UNDO.timer" >/dev/null 2>&1 || true
    install -m 644 "$SRC/ruletaker-firewall.service" "$UNIT"
    "$SYSTEMCTL" daemon-reload
    "$SYSTEMCTL" enable ruletaker-firewall.service >/dev/null 2>&1
    "$SYSTEMCTL" restart ruletaker-firewall.service   # (loads the same table again: tracked by systemd from now on)
    if loaded; then
      echo "✅ 机器防火墙（第二道）：进来的连接只放行 Google IAP 通道的 SSH，重启后照样有效"
    else
      echo "⚠️ 机器防火墙确认后不在了：再运行一次 bash deploy/gcp/rt.sh lockdown"
      exit 1
    fi
    ;;
  remove)
    "$SYSTEMCTL" stop "$UNDO.timer" >/dev/null 2>&1 || true
    "$SYSTEMCTL" disable --now ruletaker-firewall.service >/dev/null 2>&1 || true
    loaded && "$NFT" delete table "${TABLE[@]}"
    rm -f "$UNIT" "$RULES"
    "$SYSTEMCTL" daemon-reload >/dev/null 2>&1 || true
    if loaded; then echo "⚠️ 机器防火墙没拿掉"; exit 1; fi
    echo "机器防火墙已拿掉"
    ;;
  status)
    if ! loaded; then
      echo off
    elif "$SYSTEMCTL" is-enabled ruletaker-firewall.service >/dev/null 2>&1; then
      echo on
    else
      echo pending
    fi
    ;;
  *)
    echo "用法：bash vm-firewall.sh arm | confirm | remove | status"
    exit 2
    ;;
esac
