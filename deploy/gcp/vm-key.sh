#!/usr/bin/env bash
# Runs ON the VM as root: the private key kept encrypted instead of in plain text in the settings file. systemd-creds encrypts
# it with this machine's own key (/var/lib/systemd/credential.secret, root's alone) and — where the machine has one, as this
# project's Shielded VMs do — its virtual TPM, so a copy of the disk taken elsewhere cannot open it. systemd opens it for the
# service alone (LoadCredentialEncrypted= in ruletaker.service), into a directory of the service's own in memory that only
# the service's user reads and that is gone when the service stops. The settings file then holds no key: opening it (rt.sh
# edit) shows none.
#
#   bash vm-key.sh move       the settings file's PREDICT_PRIVATE_KEY, if it holds one, kept encrypted and taken out of the
#                             file (vm-setup.sh runs it on every deploy / update / env)
#   bash vm-key.sh apply      after the settings file was edited (rt.sh edit): move, check the settings, restart the service
#   bash vm-key.sh set        a new key from standard input (rt.sh key): its address, and on chain whether it controls
#                             PREDICT_ACCOUNT, checked before anything changes; kept encrypted; the service restarted on it
#   bash vm-key.sh run ARGS   the service's command line (plan, check, …) run the way the service runs: its user, its key
#   bash vm-key.sh status     where the key is kept (never the key)
#
# Every change is proved before it is made: the encrypted key is opened by systemd the way the service's will be, and the
# address it gives compared with the key's own; anything wrong leaves everything as it was. The key never touches the disk
# in plain text, a command line or the screen; the settings file's old copy is overwritten when it is replaced.
#
# For a test elsewhere: RT_ENV (the settings file), RT_STORE (the encrypted key), RT_UNIT (the service's unit file), RT_CREDS
# (systemd-creds), RT_SYSTEMD_RUN (systemd-run), RT_SYSTEMCTL (systemctl), RT_RUN (the command line), RT_AS (how the
# service's user runs a command: "sudo -u ruletaker"; "" runs it as is), RT_TPM=1 (a TPM to try), RT_WAIT (seconds a
# restart is given) stand in for the VM's own.
set -uo pipefail
umask 077
F="${RT_ENV:-/etc/ruletaker/ruletaker.env}"
STORE="${RT_STORE:-/etc/credstore.encrypted/predict_private_key}"
UNIT="${RT_UNIT:-/etc/systemd/system/ruletaker.service}"
NAME=predict_private_key
CREDS="${RT_CREDS:-systemd-creds}"
SDRUN="${RT_SYSTEMD_RUN:-systemd-run}"
SYSTEMCTL="${RT_SYSTEMCTL:-systemctl}"
WAIT="${RT_WAIT:-3}"
read -r -a AS <<< "${RT_AS-sudo -u ruletaker}"
if [ -n "${RT_RUN:-}" ]; then
  read -r -a RUN <<< "$RT_RUN"
elif [ "$(cat /opt/ruletaker/IMPL 2>/dev/null || echo rust)" = python ]; then
  RUN=(env PYTHONPATH=/opt/ruletaker/python /opt/ruletaker/python/venv/bin/python -m ruletaker)
else
  RUN=(/opt/ruletaker/bin/ruletaker)
fi
MARK="# PREDICT_PRIVATE_KEY：已加密保存在这台机器上（$STORE，只有这台机器能解开），这里不再放明文；更换私钥：bash deploy/gcp/rt.sh key"
KIND=""
ADDR=""
BEFORE=""
cd / || exit 1   # (the service's user runs commands from here: a folder it may not enter would stop Python)
SCRATCH="$(mktemp -d -p /dev/shm 2>/dev/null || mktemp -d)"   # (in memory where there is a /dev/shm: what goes here is the key's)
trap 'find "$SCRATCH" -type f -exec shred -u {} + 2>/dev/null; rm -rf "$SCRATCH"' EXIT

setting() {  # setting NAME: its value as the service reads it ("" when not set) — the same reading as vm-call.sh's
  [ -r "$F" ] || return 0
  awk -v want="$1" -v sq="'" '
    {
      line = $0
      gsub(/^[ \t\r\n\f\v]+|[ \t\r\n\f\v]+$/, "", line)
      if (line == "" || substr(line, 1, 1) == "#" || index(line, "=") == 0) next
      if (substr(line, 1, 7) == "export ") line = substr(line, 8)
      i = index(line, "=")
      name = substr(line, 1, i - 1)
      value = substr(line, i + 1)
      gsub(/^[ \t\r\n\f\v]+|[ \t\r\n\f\v]+$/, "", name)
      gsub(/^[ \t\r\n\f\v]+|[ \t\r\n\f\v]+$/, "", value)
      if (name != want) next
      q = substr(value, 1, 1)
      if (index(value, "#") > 0 && q != sq && q != "\"") {
        j = index(value, " #")
        if (j > 0) value = substr(value, 1, j - 1)
        gsub(/[ \t\r\n\f\v]+$/, "", value)
      }
      n = length(value)
      if (n >= 2 && (q == sq || q == "\"") && substr(value, n, 1) == q) value = substr(value, 2, n - 2)
      print value
      exit
    }' "$F"
}

kind_of() {  # kind_of FILE: what an encrypted key was sealed with (the first 16 bytes of systemd's format name it)
  local id
  id="$(head -c 24 "$1" 2>/dev/null | base64 -d 2>/dev/null | head -c 16 | od -An -tx1 | tr -d ' \n')"
  case "$id" in
    93a894094874449090caf2fc93cab553|af4950a849134eb1a73846304ff30c05) echo "本机密钥 + 这台机器的虚拟 TPM：拷走磁盘也解不开" ;;
    0c7cc07b117645919c4b0bea08bc20fe|faf7eb9341e3412ca1a436f95a29362f) echo "这台机器的虚拟 TPM：拷走磁盘也解不开" ;;
    5a1c6a86df9d4096b1d5a65e0862f19a) echo "本机密钥：root 才读得到；这台机器的 TPM 没用上" ;;
    *) echo "systemd-creds" ;;
  esac
}

as_service() {  # as_service [PROPERTY...] -- COMMAND...: run as the service is — its user, its devices, systemd opening its key
  local props=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do props+=(-p "$1"); shift; done
  shift
  "$SDRUN" --quiet --pipe --wait --collect --service-type=exec \
    -p User=ruletaker -p Group=ruletaker -p WorkingDirectory=/var/lib/ruletaker \
    -p Environment=STATE_DB=/var/lib/ruletaker/ruletaker.sqlite3 -p PrivateDevices=yes \
    ${props[@]+"${props[@]}"} "$@" < /dev/null
}

unit_ready() {  # the installed service opens a kept key (its unit names it); else a key taken out of the file would be lost
  if grep -qE "^LoadCredentialEncrypted=$NAME(:|$)" "$UNIT" 2>/dev/null; then return 0; fi
  echo "⚠️ 服务的 systemd 配置还不会打开加密保存的私钥（$UNIT 是旧的）：先运行 bash deploy/gcp/rt.sh update。私钥照旧留在设置文件"
  return 1
}

keep() {  # keep KEY: KEY kept encrypted in STORE — first proved to open, the way the service's will, to KEY's own address
  local key="$1" want got err how tmp
  case "$key" in 0x*) ;; *) key="0x$key" ;; esac
  if [[ ! "$key" =~ ^0x[0-9a-fA-F]{64}$ ]]; then echo "❌ 不是 64 位十六进制的私钥：什么都没改"; return 1; fi
  if ! command -v "$CREDS" >/dev/null 2>&1; then
    echo "⚠️ 这台机器没有 systemd-creds（要 systemd 250 以上，Ubuntu 24.04 自带）：私钥照旧留在设置文件"
    return 1
  fi
  want="$(printf '%s\n' "$key" | ${AS[@]+"${AS[@]}"} "${RUN[@]}" key --stdin 2>&1 | head -1)"
  if [[ ! "$want" =~ ^0x[0-9a-fA-F]{40}$ ]]; then echo "❌ 算不出这把私钥的地址（${want:-没有回答}）：什么都没改"; return 1; fi
  install -d -m 700 "$(dirname "$STORE")"
  ADDR="$want"
  BEFORE=""
  if [ -s "$STORE" ]; then   # (the one kept so far, by its address: said if this replaces it)
    BEFORE="$(as_service "LoadCredentialEncrypted=$NAME:$STORE" -- "${RUN[@]}" key 2>/dev/null | head -1)"
  fi
  tmp="$STORE.new"
  for how in host+tpm2 host; do
    if [ "$how" = host+tpm2 ] && [ ! -e /dev/tpmrm0 ] && [ -z "${RT_TPM:-}" ]; then continue; fi
    rm -f "$tmp"
    # (no PCRs: bound to this machine's TPM, not to its boot — a kernel or firmware update never locks the key out)
    if ! err="$(printf '%s' "$key" | "$CREDS" encrypt --name="$NAME" --with-key="$how" --tpm2-pcrs= - "$tmp" 2>&1)"; then
      echo "   （$how 没做成：$(printf '%s' "$err" | tail -1)）"
      continue
    fi
    chmod 600 "$tmp"
    got="$(as_service "LoadCredentialEncrypted=$NAME:$tmp" -- "${RUN[@]}" key 2>&1 | head -1)"
    if [ "$got" = "$want" ]; then
      mv -f "$tmp" "$STORE"
      KIND="$(kind_of "$STORE")"
      return 0
    fi
    echo "   （$how：systemd 替服务打开后对不上：${got:-没有回答}）"
  done
  rm -f "$tmp"
  echo "❌ 加密保存没做成（原因见上）：什么都没改"
  return 1
}

strip_plain() {  # strip_plain [KEY]: the settings file without its plain key — every PREDICT_PRIVATE_KEY line (commented-out
  # ones too) and every line holding KEY's digits, a note in the first one's place; the old file's bytes overwritten
  local key="${1:-}" pat tmp
  key="${key#0x}"
  pat="$SCRATCH/pattern"
  printf '%s\n' "$key" > "$pat"   # (read by awk from a root-only file: never on a command line)
  tmp="$(mktemp "$(dirname "$F")/.ruletaker.env.XXXXXX")" || return 1
  awk -v mark="$MARK" -v patfile="$pat" '
    BEGIN { getline key < patfile; key = tolower(key) }
    {
      t = $0
      sub(/^[ \t\r]+/, "", t)
      bare = t
      sub(/^#+[ \t]*/, "", bare)
      sub(/^export[ \t]+/, "", bare)
      i = index(bare, "=")
      name = (i > 0) ? substr(bare, 1, i - 1) : ""
      sub(/[ \t]+$/, "", name)
      if (index(t, "# PREDICT_PRIVATE_KEY：已加密保存") == 1 || name == "PREDICT_PRIVATE_KEY" || (key != "" && index(tolower($0), key) > 0)) {
        if (!done) { print mark; done = 1 }
        next
      }
      print
    }' "$F" > "$tmp" || { rm -f "$tmp"; return 1; }
  if cmp -s "$F" "$tmp"; then rm -f "$tmp"; return 0; fi   # (nothing to take out: the file left as it is)
  chmod --reference="$F" "$tmp" && chown --reference="$F" "$tmp" || { rm -f "$tmp"; return 1; }
  ln -f "$F" "$F.old" && mv -f "$tmp" "$F" || { rm -f "$tmp" "$F.old"; return 1; }
  shred -u "$F.old" 2>/dev/null || rm -f "$F.old"
}

has_plain_lines() {  # the settings file names PREDICT_PRIVATE_KEY anywhere (a line of its own, commented out or not)
  grep -qE '^[[:space:]]*#*[[:space:]]*(export[[:space:]]+)?PREDICT_PRIVATE_KEY[[:space:]]*=' "$F" 2>/dev/null
}

move() {  # the settings file's key kept encrypted, and taken out of it
  [ -f "$F" ] || return 0
  local key
  key="$(setting PREDICT_PRIVATE_KEY)"
  if [ -z "$key" ]; then
    # none to move; a kept key, and lines naming the old one still in the file (an empty one, a commented-out copy): gone
    if [ -s "$STORE" ] && has_plain_lines && unit_ready; then
      strip_plain "" && echo "✅ 设置文件里残留的 PREDICT_PRIVATE_KEY 行已删掉（私钥在加密保存里）"
    fi
    return 0
  fi
  unit_ready || return 1
  echo "把设置文件里的私钥改成加密保存…"
  keep "$key" || return 1
  if ! strip_plain "$key"; then
    echo "⚠️ 私钥已加密保存，但设置文件里的明文没删掉：再运行一次 bash deploy/gcp/rt.sh update"
    return 1
  fi
  echo "✅ 私钥（签名地址 $ADDR）已从设置文件收走，加密保存在这台机器上（$KIND）。打开设置文件不会再看到它；换私钥：bash deploy/gcp/rt.sh key"
  if [[ "$BEFORE" =~ ^0x[0-9a-fA-F]{40}$ ]] && [ "$BEFORE" != "$ADDR" ]; then
    echo "⚠️ 注意：设置文件里这把和原来加密保存的不是同一把（原来的签名地址 $BEFORE），现在用的是设置文件里这把"
  fi
}

restart() {
  "$SYSTEMCTL" restart ruletaker
  sleep "$WAIT"
  if "$SYSTEMCTL" is-active --quiet ruletaker; then
    echo "✅ 服务已重启"
  else
    echo "❌ 服务没起来：sudo journalctl -u ruletaker -n 50 看原因（bash deploy/gcp/rt.sh logs）"
    return 1
  fi
}

run_cli() {  # the command line, as the service runs it (its kept key, if there is one)
  if [ -s "$STORE" ]; then
    as_service "LoadCredentialEncrypted=$NAME:$STORE" -- "${RUN[@]}" "$@"
  else
    as_service -- "${RUN[@]}" "$@"
  fi
}

case "${1:-}" in
  move)
    move
    ;;
  apply)
    move || echo "   （设置照旧检查、照旧重启）"
    if run_cli plan --env "$F" > /dev/null; then
      restart && echo "已保存，服务按新设置运行"
    else
      echo "!! 设置文件有误（见上面），服务没有重启、还在按原来的设置运行：再运行 bash deploy/gcp/rt.sh edit 改正"
      exit 1
    fi
    ;;
  set)
    IFS= read -r key || true
    key="${key//[[:space:]]/}"
    [ -n "$key" ] || { echo "❌ 没收到私钥：什么都没改"; exit 1; }
    case "$key" in 0x*) ;; *) key="0x$key" ;; esac
    [[ "$key" =~ ^0x[0-9a-fA-F]{64}$ ]] || { echo "❌ 不是 64 位十六进制的私钥：什么都没改"; exit 1; }
    unit_ready || exit 1
    echo "核对新私钥（它的签名地址，和它在链上是不是 PREDICT_ACCOUNT 的控制钥匙）…"
    out="$(printf '%s\n' "$key" | ${AS[@]+"${AS[@]}"} "${RUN[@]}" key --stdin --env "$F" 2>&1)"
    code=$?
    printf '%s\n' "$out" | sed 's/^/   /'
    case "$code" in
      0) ;;
      1) echo "❌ 这把私钥控制不了设置里的 PREDICT_ACCOUNT：什么都没改（服务照旧用原来的私钥）"; exit 1 ;;
      3) echo "❌ 链上核对没做成：什么都没改，稍后再运行一次 bash deploy/gcp/rt.sh key"; exit 1 ;;
      *) echo "❌ 什么都没改"; exit 1 ;;
    esac
    keep "$key" || exit 1
    if [ -f "$F" ] && ! strip_plain "$key"; then
      # (a different plain key left in the file would stop the service at its next start: not restarted on it)
      echo "❌ 新私钥已加密保存，但设置文件里的旧私钥没删掉：服务没有重启（还在用原来的）。再运行一次 bash deploy/gcp/rt.sh key"
      exit 1
    fi
    key=""
    echo "✅ 新私钥（签名地址 $ADDR）已加密保存在这台机器上（$KIND），设置文件里没有明文"
    [[ "$BEFORE" =~ ^0x[0-9a-fA-F]{40}$ ]] && [ "$BEFORE" != "$ADDR" ] && echo "   （换掉了原来加密保存的那把：签名地址 $BEFORE）"
    restart || exit 1
    ;;
  run)
    shift
    run_cli "$@"
    ;;
  status)
    if [ -s "$STORE" ]; then
      echo "私钥：加密保存在 $STORE（$(kind_of "$STORE")）"
    else
      echo "私钥：这台机器上没有加密保存的私钥"
    fi
    if [ -n "$(setting PREDICT_PRIVATE_KEY)" ]; then
      echo "⚠️ 设置文件 $F 里有明文的 PREDICT_PRIVATE_KEY：bash deploy/gcp/rt.sh update 改成加密保存"
    fi
    ;;
  *)
    echo "用法：bash vm-key.sh move | apply | set（私钥从标准输入）| run 参数… | status"
    exit 2
    ;;
esac
