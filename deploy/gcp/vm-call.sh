#!/usr/bin/env bash
# Runs ON the VM as root (rt.sh pipes it in): one call on the service's own pages (127.0.0.1), with STATUS_KEY and STATUS_PORT
# read from the settings file the way the service reads it — the first line of a name wins; "export ", quotes around the value
# and a " #" comment after it taken off — and the key handed to curl through a pipe, never on a command line (ps shows those).
#
#   bash -s -- port                          the console's port
#   bash -s -- get /status | /status.json
#   bash -s -- post /pause | /resume | /mode?to=off|paper|on
#   bash -s -- control check | test          the self-check / the order test (the command line's check when the service is down)
#   bash -s -- probe-keys                    what a probe machine needs (the API key, the chain) — never the private key
#
# Nothing here reads its own stdin (the script itself comes that way). For a test elsewhere: RT_ENV stands in for the VM's
# settings file, RT_RUN for the command line of the version in use.
set -uo pipefail
F="${RT_ENV:-/etc/ruletaker/ruletaker.env}"

setting() {  # setting NAME: its value as the service reads it ("" when not set)
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

port="$(setting STATUS_PORT)"
case "$port" in ''|*[!0-9]*) port=8080 ;; esac

run_cli() {  # the command line of the version in use
  if [ "$(cat /opt/ruletaker/IMPL 2>/dev/null || echo rust)" = python ]; then
    echo "env PYTHONPATH=/opt/ruletaker/python /opt/ruletaker/python/venv/bin/python -m ruletaker"
  else
    echo /opt/ruletaker/bin/ruletaker
  fi
}

BODY=""
CODE=""
call() {  # call METHOD PATH [JSON] [SECONDS]: the answer in BODY, the HTTP status in CODE ("000": no connection)
  local key out
  key="$(setting STATUS_KEY)"
  key="${key//\\/\\\\}"
  key="${key//\"/\\\"}"
  out="$(mktemp)"
  local args=(-sS -m "${4:-15}" -o "$out" -w '%{http_code}' -X "$1" -H 'X-RT: 1' -K -)
  [ -z "${3:-}" ] || args+=(-H 'Content-Type: application/json' --data-binary "$3")
  CODE="$(printf 'header = "X-Status-Key: %s"\n' "$key" | curl "${args[@]}" "http://127.0.0.1:${port}$2" 2>/dev/null)"
  BODY="$(cat "$out" 2>/dev/null)"
  rm -f "$out"
}

said() {  # what went wrong, in words (never silent)
  case "$CODE" in
    000|"") echo "❌ 连不上服务的控制台（127.0.0.1:${port}）：服务没在运行，或 STATUS_PORT 设成了 0。看原因：sudo systemctl status ruletaker" ;;
    403) echo "❌ 控制台拒绝了（403）：STATUS_KEY 和服务正在用的对不上——改过设置文件但服务没重启？（sudo systemctl restart ruletaker）" ;;
    *) echo "❌ 控制台答复 HTTP ${CODE}：${BODY}" ;;
  esac
}

message() {  # the "message" of a JSON answer (the answer itself when there is none)
  if command -v python3 >/dev/null 2>&1; then
    printf '%s' "$BODY" | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin)["message"])
except Exception:
    sys.exit(1)' 2>/dev/null || printf '%s\n' "$BODY"
  else
    printf '%s\n' "$BODY"
  fi
}

case "${1:-}" in
  port)
    echo "$port"
    ;;
  probe-keys)
    for name in RULE_API_KEY PREDICT_API_KEY PREDICT_CHAIN_ID; do
      value="$(setting "$name")"
      [ -z "$value" ] || printf '%s=%s\n' "$name" "$value"
    done
    ;;
  get|post)
    method="$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')"
    case "$1 ${2:-}" in
      "get /status"|"get /status.json"|"post /pause"|"post /resume"|"post /mode?to=off"|"post /mode?to=paper"|"post /mode?to=on") ;;
      *) echo "❌ 不认识的调用：$*"; exit 2 ;;
    esac
    call "$method" "$2"
    if [ "$CODE" = 200 ]; then printf '%s\n' "$BODY"; else said; exit 1; fi
    ;;
  control)
    case "${2:-}" in check|test) ;; *) echo "❌ 不认识的操作：${2:-}"; exit 2 ;; esac
    call POST /api/control "{\"action\":\"$2\"}" 90
    if [ "$CODE" = 200 ]; then
      message
    elif [ "$2" = check ] && [ "$CODE" = 000 ]; then
      echo "（服务没在运行：改用命令行自检）"
      if [ -n "${RT_RUN:-}" ]; then
        $RT_RUN check --env "$F" < /dev/null
      else
        # shellcheck disable=SC2046
        sudo -u ruletaker $(run_cli) check --env "$F" < /dev/null
      fi
    else
      said
      [ "$2" = test ] && echo "   挂单测试要服务在运行：先把上面的问题解决，再运行 bash deploy/gcp/rt.sh test"
      exit 1
    fi
    ;;
  *)
    echo "用法：bash vm-call.sh port | get PATH | post PATH | control check|test | probe-keys"
    exit 2
    ;;
esac
