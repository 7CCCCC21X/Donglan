#!/usr/bin/env bash
# Runs ON the VM as root: the private key — and every other secret of the settings file (the API keys, the Telegram tokens,
# STATUS_KEY, BSC_RPC_URL: SECRETS below) — kept encrypted instead of in plain text in the settings file. systemd-creds encrypts
# it with this machine's own key (/var/lib/systemd/credential.secret, root's alone) and — where the machine has one, as this
# project's Shielded VMs do — its virtual TPM, so a copy of the disk taken elsewhere cannot open it. systemd opens it for the
# service alone (LoadCredentialEncrypted= in ruletaker.service), into a directory of the service's own in memory that only
# the service's user reads and that is gone when the service stops. The settings file then holds no key: opening it (rt.sh
# edit) shows none.
#
#   bash vm-key.sh move       the settings file's PREDICT_PRIVATE_KEY, and its secrets (SECRETS), if it holds any, kept
#                             encrypted and taken out of the file (vm-setup.sh runs it on every deploy / update / env)
#   bash vm-key.sh apply      after the settings file was edited (rt.sh edit): move, check the settings, restart the service
#   bash vm-key.sh set        a new key from standard input (rt.sh key): its address, and on chain whether it controls
#                             PREDICT_ACCOUNT, checked before anything changes; kept encrypted; the service restarted on it
#   bash vm-key.sh put NAME   one setting (its value from standard input; rt.sh set) replaced — every line of it — or added,
#                             checked the way the service reads it, the service restarted; a check that fails puts it back. A
#                             secret (SECRETS) goes to the encrypted store, never into the file; an empty one is taken out
#   bash vm-key.sh run ARGS   the service's command line (plan, check, …) run the way the service runs: its user, its key
#   bash vm-key.sh status     where the key and the secrets are kept (never a value)
#
# Every change is proved before it is made: the encrypted key is opened by systemd the way the service's will be, and the
# address it gives compared with the key's own; anything wrong leaves everything as it was. The key never touches the disk
# in plain text, a command line or the screen; the settings file's old copy is overwritten when it is replaced.
#
# For a test elsewhere: RT_ENV (the settings file), RT_STORE (the encrypted key), RT_SSTORE (the encrypted secrets), RT_UNIT
# (the service's unit file), RT_CREDS
# (systemd-creds), RT_SYSTEMD_RUN (systemd-run), RT_SYSTEMCTL (systemctl), RT_RUN (the command line), RT_AS (how the
# service's user runs a command: "sudo -u ruletaker"; "" runs it as is), RT_TPM=1 (a TPM to try), RT_WAIT (seconds a
# restart is given) stand in for the VM's own.
set -uo pipefail
umask 077
F="${RT_ENV:-/etc/ruletaker/ruletaker.env}"
STORE="${RT_STORE:-/etc/credstore.encrypted/predict_private_key}"
UNIT="${RT_UNIT:-/etc/systemd/system/ruletaker.service}"
NAME=predict_private_key
SSTORE="${RT_SSTORE:-/etc/credstore.encrypted/ruletaker_secrets}"
SNAME=ruletaker_secrets
# (src/config.rs SECRET_SETTINGS: the service takes these from the encrypted store when the settings file sets none)
SECRETS=(PREDICT_API_KEY RULE_API_KEY RULE_API_KEY_2 RULE_API_KEY_3 RULE_API_KEY_4 RULE_API_KEY_5 RULE_API_KEY_6 RULE_API_KEY_7
  RULE_API_KEY_8 RULE_API_KEY_9 TELEGRAM_BOT_TOKEN TELEGRAM_COMMAND_TOKEN STATUS_KEY BSC_RPC_URL)
SECRETS_RE='PREDICT_API_KEY|RULE_API_KEY(_[2-9])?|TELEGRAM_BOT_TOKEN|TELEGRAM_COMMAND_TOKEN|STATUS_KEY|BSC_RPC_URL'
SMARK="# 下面这些设置已加密保存在这台机器上（$SSTORE，只有这台机器能解开），这里不再放明文——改用 bash deploy/gcp/rt.sh set 名字："
SKIND=""
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
  as_service_in "$@" < /dev/null
}

as_service_in() {  # as_service with this standard input (a key for the command to check)
  local props=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do props+=(-p "$1"); shift; done
  shift
  "$SDRUN" --quiet --pipe --wait --collect --service-type=exec \
    -p User=ruletaker -p Group=ruletaker -p WorkingDirectory=/var/lib/ruletaker \
    -p Environment=STATE_DB=/var/lib/ruletaker/ruletaker.sqlite3 -p PrivateDevices=yes \
    ${props[@]+"${props[@]}"} "$@"
}

kept_props() {  # the LoadCredentialEncrypted= properties of what is kept (the key, the secrets), one a line
  [ -s "$STORE" ] && echo "LoadCredentialEncrypted=$NAME:$STORE"
  [ -s "$SSTORE" ] && echo "LoadCredentialEncrypted=$SNAME:$SSTORE"
  return 0
}

secrets_unit_ready() {  # the installed service opens kept secrets (its unit names them); else secrets taken out would be lost
  if grep -qE "^LoadCredentialEncrypted=$SNAME(:|$)" "$UNIT" 2>/dev/null; then return 0; fi
  echo "⚠️ 服务的 systemd 配置还不会打开加密保存的设置（$UNIT 是旧的）：先运行 bash deploy/gcp/rt.sh update。Key、令牌照旧留在设置文件"
  return 1
}

opened_secrets() {  # opened_secrets FILE: the kept secrets (NAME=VALUE lines) into FILE — root's, in memory; none kept: empty
  : > "$1"
  [ -s "$SSTORE" ] || return 0
  if ! "$CREDS" decrypt --name="$SNAME" "$SSTORE" - > "$1" 2>/dev/null; then
    : > "$1"
    echo "❌ 加密保存的设置这台机器解不开（$SSTORE）：什么都没改"
    return 1
  fi
}

kept_names() {  # the names of the kept secrets, in SECRETS' order (never a value)
  local have="$SCRATCH/names" n out=()
  opened_secrets "$have" > /dev/null || return 0
  for n in "${SECRETS[@]}"; do grep -q "^$n=" "$have" && out+=("$n"); done
  shred -u "$have" 2>/dev/null || rm -f "$have"
  echo "${out[*]}"
}

merge_secrets() {  # merge_secrets OUT NEW: the kept secrets with NEW's NAME=VALUE lines over them (an empty value takes its
  # name out), in SECRETS' order, into OUT
  local out="$1" add="$2" have="$SCRATCH/have" n v
  opened_secrets "$have" || return 1
  : > "$out"
  for n in "${SECRETS[@]}"; do
    if grep -q "^$n=" "$add"; then v="$(sed -n "s/^$n=//p" "$add" | head -1)"; else v="$(sed -n "s/^$n=//p" "$have" | head -1)"; fi
    if [ -n "$v" ]; then printf '%s=%s\n' "$n" "$v" >> "$out"; fi
  done
  v=""
  shred -u "$have" 2>/dev/null || rm -f "$have"
}

seal_secrets() {  # seal_secrets FILE: FILE's lines kept encrypted in SSTORE — first proved to open, the way the service's will,
  # to the very same bytes; an empty FILE: nothing kept (the store taken away)
  local src="$1" tmp="$SSTORE.new" how err
  install -d -m 700 "$(dirname "$SSTORE")"
  if [ ! -s "$src" ]; then rm -f "$SSTORE"; SKIND=""; return 0; fi
  if ! command -v "$CREDS" >/dev/null 2>&1; then
    echo "⚠️ 这台机器没有 systemd-creds（要 systemd 250 以上，Ubuntu 24.04 自带）：Key、令牌照旧留在设置文件"
    return 1
  fi
  for how in host+tpm2 host; do
    if [ "$how" = host+tpm2 ] && [ ! -e /dev/tpmrm0 ] && [ -z "${RT_TPM:-}" ]; then continue; fi
    rm -f "$tmp"
    # (no PCRs: bound to this machine's TPM, not to its boot — a kernel or firmware update never locks them out)
    if ! err="$("$CREDS" encrypt --name="$SNAME" --with-key="$how" --tpm2-pcrs= "$src" "$tmp" 2>&1)"; then
      echo "   （$how 没做成：$(printf '%s' "$err" | tail -1)）"
      continue
    fi
    chmod 600 "$tmp"
    # shellcheck disable=SC2016  # (expanded by the shell systemd starts for the service's user, not by this one)
    as_service "LoadCredentialEncrypted=$SNAME:$tmp" -- /bin/sh -c 'cat "$CREDENTIALS_DIRECTORY/'"$SNAME"'"' > "$SCRATCH/opened" 2>/dev/null
    if cmp -s "$SCRATCH/opened" "$src"; then
      shred -u "$SCRATCH/opened" 2>/dev/null || rm -f "$SCRATCH/opened"
      mv -f "$tmp" "$SSTORE"
      SKIND="$(kind_of "$SSTORE")"
      return 0
    fi
    shred -u "$SCRATCH/opened" 2>/dev/null || rm -f "$SCRATCH/opened"
    echo "   （$how：systemd 替服务打开后对不上）"
  done
  rm -f "$tmp"
  echo "❌ Key、令牌的加密保存没做成（原因见上）：什么都没改"
  return 1
}

strip_secrets() {  # strip_secrets [NEW [keep]]: the settings file without the secrets — every line naming one of SECRETS
  # (commented-out ones too) and every line holding one of NEW's values — a note naming what is kept in the first one's
  # place; the old file overwritten, or with "keep" kept as F.prev (a hard link) to put back or overwrite after
  local add="${1:-/dev/null}" keep="${2:-}" names mark tmp
  names="$(kept_names)"
  mark=""
  [ -z "$names" ] || mark="$SMARK$names"
  tmp="$(mktemp "$(dirname "$F")/.ruletaker.env.XXXXXX")" || return 1
  awk -v mark="$mark" -v sprefix="${SMARK%%（*}" -v re="^($SECRETS_RE)\$" -v valfile="$add" '
    BEGIN { while ((getline line < valfile) > 0) { j = index(line, "="); v = substr(line, j + 1); if (length(v) >= 8) V[v] = 1 } }
    {
      t = $0
      sub(/^[ \t\r]+/, "", t)
      bare = t
      sub(/^#+[ \t]*/, "", bare)
      sub(/^export[ \t]+/, "", bare)
      i = index(bare, "=")
      name = (i > 0) ? substr(bare, 1, i - 1) : ""
      sub(/[ \t]+$/, "", name)
      hit = (index(t, sprefix) == 1) || name ~ re
      if (!hit) for (v in V) if (index($0, v) > 0) { hit = 1; break }
      if (hit) {
        if (!done && mark != "") print mark
        done = 1
        next
      }
      print
    }' "$F" > "$tmp" || { rm -f "$tmp"; return 1; }
  if cmp -s "$F" "$tmp"; then rm -f "$tmp"; [ -z "$keep" ] || ln -f "$F" "$F.prev"; return 0; fi
  chmod --reference="$F" "$tmp" && chown --reference="$F" "$tmp" || { rm -f "$tmp"; return 1; }
  if [ -n "$keep" ]; then
    ln -f "$F" "$F.prev" && mv -f "$tmp" "$F" || { rm -f "$tmp" "$F.prev"; return 1; }
    return 0
  fi
  ln -f "$F" "$F.old" && mv -f "$tmp" "$F" || { rm -f "$tmp" "$F.old"; return 1; }
  shred -u "$F.old" 2>/dev/null || rm -f "$F.old"
}

has_secret_lines() {  # the settings file names one of SECRETS anywhere (a line of its own, commented out or not)
  grep -qE "^[[:space:]]*#*[[:space:]]*(export[[:space:]]+)?($SECRETS_RE)[[:space:]]*=" "$F" 2>/dev/null
}

move_secrets() {  # the settings file's secrets kept encrypted (beside those kept already), and taken out of it
  [ -f "$F" ] || return 0
  local n v found=() add="$SCRATCH/add" merged="$SCRATCH/merged"
  : > "$add"
  for n in "${SECRETS[@]}"; do
    v="$(setting "$n")"
    if [ -n "$v" ]; then printf '%s=%s\n' "$n" "$v" >> "$add"; found+=("$n"); fi
  done
  v=""
  if [ ${#found[@]} -eq 0 ]; then
    # none to move; secrets kept, and lines naming them still in the file (empty ones, commented-out copies): gone
    if [ -s "$SSTORE" ] && has_secret_lines && secrets_unit_ready; then
      strip_secrets && echo "✅ 设置文件里残留的 Key、令牌行已删掉（它们在加密保存里）"
    fi
    return 0
  fi
  secrets_unit_ready || return 1
  echo "把设置文件里的 Key、令牌（${found[*]}）改成加密保存…"
  merge_secrets "$merged" "$add" || return 1
  seal_secrets "$merged" || return 1
  if ! strip_secrets "$add"; then
    echo "⚠️ 已加密保存，但设置文件里的明文没删掉：再运行一次 bash deploy/gcp/rt.sh update"
    return 1
  fi
  echo "✅ ${found[*]} 已从设置文件收走，加密保存在这台机器上（$SKIND）。打开设置文件不会再看到它们；改用 bash deploy/gcp/rt.sh set 名字"
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

put_setting() {  # put_setting NAME: every line setting NAME (as the service reads names) gone, NAME=<the value in SCRATCH/value>
  # in the first one's place, else at the end; the old file kept as F.prev (a hard link) to put back or overwrite after
  local name="$1" tmp
  tmp="$(mktemp "$(dirname "$F")/.ruletaker.env.XXXXXX")" || return 1
  awk -v want="$name" -v vfile="$SCRATCH/value" '
    BEGIN { v = ""; getline v < vfile }
    {
      line = $0
      gsub(/^[ \t\r\n\f\v]+|[ \t\r\n\f\v]+$/, "", line)
      name = ""
      if (line != "" && substr(line, 1, 1) != "#" && index(line, "=") > 0) {
        if (substr(line, 1, 7) == "export ") line = substr(line, 8)
        name = substr(line, 1, index(line, "=") - 1)
        gsub(/^[ \t\r\n\f\v]+|[ \t\r\n\f\v]+$/, "", name)
      }
      if (name == want) {
        if (!done) { print want "=" v; done = 1 }
        next
      }
      print
    }
    END { if (!done) print want "=" v }' "$F" > "$tmp" || { rm -f "$tmp"; return 1; }
  chmod --reference="$F" "$tmp" && chown --reference="$F" "$tmp" || { rm -f "$tmp"; return 1; }
  ln -f "$F" "$F.prev" && mv -f "$tmp" "$F" || { rm -f "$tmp" "$F.prev"; return 1; }
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

run_cli() {  # the command line, as the service runs it (its kept key and secrets, if there are any)
  local props=()
  mapfile -t props < <(kept_props)
  as_service ${props[@]+"${props[@]}"} -- "${RUN[@]}" "$@"
}

case "${1:-}" in
  move)
    move
    key_ok=$?
    move_secrets || exit 1
    exit "$key_ok"
    ;;
  apply)
    move || echo "   （设置照旧检查、照旧重启）"
    move_secrets || echo "   （设置照旧检查、照旧重启）"
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
    if [ -s "$SSTORE" ]; then   # (BSC_RPC_URL may be kept encrypted: the check opens it the way the service would)
      out="$(printf '%s\n' "$key" | as_service_in "LoadCredentialEncrypted=$SNAME:$SSTORE" -- "${RUN[@]}" key --stdin --env "$F" 2>&1)"
    else
      out="$(printf '%s\n' "$key" | ${AS[@]+"${AS[@]}"} "${RUN[@]}" key --stdin --env "$F" 2>&1)"
    fi
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
  put)
    # (rt.sh set: from anywhere, a phone too — nano wants a Ctrl key a phone has not)
    name="${2:-}"
    [[ "$name" =~ ^[A-Z][A-Z0-9_]*$ ]] || { echo "❌ 设置名只能是大写字母、数字和下划线（例：RULE_WS_CONNECTIONS）：什么都没改"; exit 2; }
    [ "$name" != PREDICT_PRIVATE_KEY ] || { echo "❌ 私钥用 bash deploy/gcp/rt.sh key 换（会加密保存，不放进设置文件）：什么都没改"; exit 2; }
    [ -f "$F" ] || { echo "❌ 没有设置文件 $F：什么都没改"; exit 1; }
    IFS= read -r value || true
    value="${value%$'\r'}"
    case "$value" in
      *[[:space:]]*|*[[:cntrl:]]*|*\"*|*\'*|*#*|*\\*)
        echo "❌ 值里不能有空格、引号、#、反斜杠：什么都没改"; exit 2 ;;
    esac
    printf '%s' "$value" > "$SCRATCH/value"   # (read by awk from a root-only file in memory: never on a command line)
    value=""
    if [[ "$name" =~ ^($SECRETS_RE)$ ]]; then
      # a secret: into the encrypted store (an empty value takes it out), its lines gone from the settings file
      secrets_unit_ready || exit 1
      # (every secret still in the settings file in plain text taken in with it: none of them lost when their lines go)
      add="$SCRATCH/add"
      : > "$add"
      for n in "${SECRETS[@]}"; do
        [ "$n" = "$name" ] && continue
        v="$(setting "$n")"
        if [ -n "$v" ]; then printf '%s=%s\n' "$n" "$v" >> "$add"; fi
      done
      v=""
      { printf '%s=' "$name"; cat "$SCRATCH/value"; printf '\n'; } >> "$add"
      [ -s "$SSTORE" ] && cp -p "$SSTORE" "$SSTORE.prev"
      merge_secrets "$SCRATCH/merged" "$add" && seal_secrets "$SCRATCH/merged" || { rm -f "$SSTORE.prev"; exit 1; }
      if ! strip_secrets "$add" keep; then
        if [ -f "$SSTORE.prev" ]; then mv -f "$SSTORE.prev" "$SSTORE"; else rm -f "$SSTORE"; fi
        echo "❌ 设置文件没写成：什么都没改"; exit 1
      fi
      if run_cli plan --env "$F" > /dev/null; then
        shred -u "$F.prev" "$SSTORE.prev" 2>/dev/null || rm -f "$F.prev" "$SSTORE.prev"
        if [ -s "$SCRATCH/value" ]; then echo "✅ $name 已加密保存在这台机器上（$SKIND），设置文件里没有明文"; else echo "✅ $name 已删掉"; fi
        restart && echo "服务按新设置运行"
      else
        if [ -f "$SSTORE.prev" ]; then mv -f "$SSTORE.prev" "$SSTORE"; else rm -f "$SSTORE"; fi
        ln -f "$F" "$F.bad" && mv -f "$F.prev" "$F"
        shred -u "$F.bad" 2>/dev/null || rm -f "$F.bad"
        echo "!! 这个值服务不认（原因见上面）：加密保存的设置和设置文件都已恢复原样，服务没有重启、照旧运行"
        exit 1
      fi
      exit 0
    fi
    put_setting "$name" || { echo "❌ 设置文件没写成：什么都没改"; exit 1; }
    if run_cli plan --env "$F" > /dev/null; then
      shred -u "$F.prev" 2>/dev/null || rm -f "$F.prev"
      echo "✅ $name 已写进设置文件"
      restart && echo "服务按新设置运行"
    else
      ln -f "$F" "$F.bad" && mv -f "$F.prev" "$F"
      shred -u "$F.bad" 2>/dev/null || rm -f "$F.bad"
      echo "!! 这个值服务不认（原因见上面）：设置文件已恢复原样，服务没有重启、照旧运行"
      exit 1
    fi
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
    names="$(kept_names)"
    if [ -n "$names" ]; then
      echo "Key、令牌：加密保存在 $SSTORE（$(kind_of "$SSTORE")）：$names"
    else
      echo "Key、令牌：这台机器上没有加密保存的"
    fi
    plain=()
    for n in "${SECRETS[@]}"; do [ -n "$(setting "$n")" ] && plain+=("$n"); done
    if [ ${#plain[@]} -gt 0 ]; then
      echo "⚠️ 设置文件 $F 里还有明文：${plain[*]}——bash deploy/gcp/rt.sh update 改成加密保存"
    fi
    ;;
  *)
    echo "用法：bash vm-key.sh move | apply | set（私钥从标准输入）| put 设置名（值从标准输入）| run 参数… | status"
    exit 2
    ;;
esac
