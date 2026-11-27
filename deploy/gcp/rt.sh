#!/usr/bin/env bash
# 规则吃单 · Google Cloud 部署和日常操作（在 Google Cloud Shell 里、仓库根目录下运行）
#
#   bash deploy/gcp/rt.sh deploy          第一次：在东京建一台虚拟机（asia-northeast1），装好、启动
#   bash deploy/gcp/rt.sh lockdown        安全加固：封掉这台机器所有对外的入站连接，SSH 只能走 Google 的 IAP 通道（要 Google 登录）；
#                                         Google Cloud 防火墙之外再加一道机器自己的防火墙（先验证能登录才保留，否则 3 分钟自动撤销）
#   bash deploy/gcp/rt.sh unlock          解除上面的加固
#   bash deploy/gcp/rt.sh nosa            去掉虚拟机的服务账号（早期部署的机器带着项目默认的；要停机约 1 分钟，会先问）
#   bash deploy/gcp/rt.sh update          代码更新后：重新上传代码、编译、跑测试、重启（设置不动）
#   RT_IMPL=python bash deploy/gcp/rt.sh update   换回 Python 版（备用）；RT_IMPL=rust 再换回 Rust 版（记录、设置都通用）
#   bash deploy/gcp/rt.sh env [文件]      上传新的设置文件（默认 ./ruletaker.env）并重启
#   bash deploy/gcp/rt.sh edit            直接在虚拟机上改设置文件（nano），检查无误才重启——私钥不用再留在 Cloud Shell
#   bash deploy/gcp/rt.sh set 名 [值]     改一项设置，不用编辑器（手机上也能用）：例 rt.sh set RULE_WS_CONNECTIONS 3；不写值就
#                                         隐藏输入（填 Key 用）。检查无误才重启，不认的值把设置恢复原样。Key、令牌（API Key、
#                                         Telegram 令牌、STATUS_KEY、BSC_RPC_URL）直接加密保存，不进设置文件；值留空就删掉
#   bash deploy/gcp/rt.sh key             换私钥：在这里粘贴（不显示），先核对它是不是 PREDICT_ACCOUNT 的控制钥匙，对了才在虚拟机上
#                                         加密保存、重启服务（私钥不在设置文件里：update / env / edit 会把设置文件里的明文自动收走）
#   bash deploy/gcp/rt.sh audit           安全体检（只读）：私钥、Key、令牌只在该在的地方（加密保存）、权限、加固、谁能登录
#   bash deploy/gcp/rt.sh console         打开网页控制台和复盘页（通过 Cloud Shell 的“网页预览”，不开公网端口）
#   bash deploy/gcp/rt.sh status          看状态（就是 huice 的 /rules）
#   bash deploy/gcp/rt.sh logs            看实时日志（Ctrl+C 退出）
#   bash deploy/gcp/rt.sh check           自检：登录、账户、余额、授权、下单接口、推送、记录（和 Telegram /check 一样）
#   bash deploy/gcp/rt.sh test            挂单测试：挂一张约 $1 的低价单、查到、撤掉（和 Telegram /test 一样）
#   bash deploy/gcp/rt.sh latency [速率]  量这台机器到 Predict 的延迟（只读）；速率＝这次测量用的每分钟请求数（例 4000：REST 每 60 ms
#                                         读一次，和推送比谁先到），不写就按设置文件的 PREDICT_RATE_PER_MIN；只给这次用，服务不动
#   bash deploy/gcp/rt.sh pause|resume    暂停 / 恢复开新单
#   bash deploy/gcp/rt.sh mode off|paper|on   运行中切换模式（不会高于设置文件里的 RULE_TAKER）
#   bash deploy/gcp/rt.sh restart|stop|start|ssh
#   bash deploy/gcp/rt.sh probe ZONE      在别的区域临时开一台小机器量延迟（推送延迟、读盘口），量完自动删除（例：asia-southeast1-b；
#                                         只带 API Key——从虚拟机的设置文件读，不带私钥）
#   bash deploy/gcp/rt.sh delete          删除虚拟机（数据一起删掉，先确认）
#
# 可用环境变量改默认值：PROJECT（默认 gcloud 当前项目）ZONE（asia-northeast1-b）VM（ruletaker）MACHINE（e2-standard-2）
#   RT_IMPL（rust | python：默认 Rust 版；换过之后一直用换成的那个，直到再指定）
set -euo pipefail

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null || true)}"
ZONE="${ZONE:-asia-northeast1-b}"
VM="${VM:-ruletaker}"
MACHINE="${MACHINE:-e2-standard-2}"
IMAGE_FAMILY="${IMAGE_FAMILY:-ubuntu-2404-lts-amd64}"
IMAGE_PROJECT="${IMAGE_PROJECT:-ubuntu-os-cloud}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TAG="ruletaker"                          # the VM's network tag: the lockdown's firewall rules apply to the machines that carry it
RULE_IAP="ruletaker-allow-iap-ssh"       # ...SSH from Google's IAP tunnel alone (35.235.240.0/20: reached only through a Google login)
RULE_DENY="ruletaker-deny-inbound"       # ...and every other connection from outside refused
IAP_RANGE="35.235.240.0/20"

die() { echo "错误：$*" >&2; exit 1; }
case "${RT_IMPL:-}" in
  ""|rust|python) ;;
  *) die "RT_IMPL 只能是 rust 或 python（现在是 ${RT_IMPL}）" ;;
esac
[ -n "$PROJECT" ] || die "没有选 Google Cloud 项目：先运行 gcloud config set project 你的项目ID"
G=(--project "$PROJECT")
Z=(--zone "$ZONE")

# After `rt.sh lockdown` the VM takes no connection from the internet, and SSH goes through the IAP tunnel. Told by the deny
# rule's existence (one look a run); RT_IAP=1 / RT_IAP=0 says it outright.
IAP=()
iap_mode() {
  case "${RT_IAP:-}" in
    1) IAP=(--tunnel-through-iap); return 0 ;;
    0) IAP=(); return 0 ;;
  esac
  if gcloud compute firewall-rules describe "$RULE_DENY" "${G[@]}" >/dev/null 2>&1; then
    IAP=(--tunnel-through-iap)
  fi
}

ssh_to() {  # ssh_to NAME ZONE [gcloud compute ssh arguments...] — through IAP for the service's own VM once it is locked down
  local name="$1" zone="$2"
  shift 2
  if [ "$name" = "$VM" ]; then
    gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet ${IAP[@]+"${IAP[@]}"} "$@"
  else
    gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet "$@"
  fi
}

kill_tree() {  # a process and everything it started (gcloud starts ssh, ssh the IAP helper)
  local p
  for p in $(pgrep -P "$1" 2>/dev/null); do kill_tree "$p"; done
  kill "$1" 2>/dev/null || true
}

remote() {  # a command on the VM
  ssh_to "$VM" "$ZONE" --command "$1"
}

wait_ssh() {
  local name="$1" zone="$2"
  echo "等虚拟机 $name 可以 SSH 登录…"
  for _ in $(seq 1 40); do
    if ssh_to "$name" "$zone" --command true >/dev/null 2>&1; then
      return 0
    fi
    sleep 6
  done
  die "虚拟机 $name 两分钟内没法 SSH 登录"
}

create_vm() {  # create_vm NAME ZONE MACHINE DISK [NETWORK TAG]
  local name="$1" zone="$2" machine="$3" disk="$4" tag="${5:-}"
  local tags=()
  [ -z "$tag" ] || tags=(--tags "$tag")
  gcloud services enable compute.googleapis.com "${G[@]}" >/dev/null
  # No service account: the machine itself can call no Google Cloud API (it needs none) — whoever got into it would find no
  # token there to reach the rest of the project with.
  gcloud compute instances create "$name" "${G[@]}" --zone "$zone" \
    --machine-type "$machine" \
    --image-family "$IMAGE_FAMILY" --image-project "$IMAGE_PROJECT" \
    --boot-disk-size "$disk" --boot-disk-type pd-balanced \
    --network-tier PREMIUM \
    --no-service-account --no-scopes \
    --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring \
    --labels app=ruletaker ${tags[@]+"${tags[@]}"}
}

upload_code() {
  local name="$1" zone="$2"
  echo "上传代码…"
  # The sources only (the Rust version and the Python one beside it); the build and its cache stay on the VM (target/, .cargo/).
  (cd "$ROOT" && tar czf - --exclude=__pycache__ --exclude=venv --exclude=target \
      Cargo.toml Cargo.lock rustfmt.toml src web tests deploy/gcp python/ruletaker python/tests python/requirements.txt python/requirements.lock) \
    | ssh_to "$name" "$zone" --command "sudo mkdir -p /opt/ruletaker && sudo rm -rf /opt/ruletaker/src /opt/ruletaker/web /opt/ruletaker/tests \
        /opt/ruletaker/deploy /opt/ruletaker/python/ruletaker /opt/ruletaker/python/tests && sudo tar xzf - -C /opt/ruletaker"
}

upload_env() {
  local file="${1:-$ROOT/ruletaker.env}"
  [ -f "$file" ] || die "找不到设置文件 $file：先 cp ruletaker.env.example ruletaker.env 再填好"
  grep -qE '^(RULE_API_KEY|PREDICT_API_KEY)=.+' "$file" || echo "提醒：$file 里 RULE_API_KEY / PREDICT_API_KEY 都是空的，paper / on 模式启动不了"
  echo "上传设置文件 $file（走 SSH 加密通道，只存在虚拟机的 /etc/ruletaker/ruletaker.env，root 所有、权限 640）…"
  ssh_to "$VM" "$ZONE" --command "umask 077; cat > /tmp/ruletaker.env" < "$file"
  ENV_UPLOADED="$file"
}

setup() {
  remote "sudo ${RT_IMPL:+RT_IMPL=$RT_IMPL }bash /opt/ruletaker/deploy/gcp/vm-setup.sh"
  echo "网页控制台：bash deploy/gcp/rt.sh console（等它显示 ✅ 再点网页预览）｜手机上随时用：Telegram 发 /menu"
}

# The command line of the version in use on the VM (its subcommands: run, check, latency, plan).
RUN_ON_VM='if [ "$(cat /opt/ruletaker/IMPL 2>/dev/null || echo rust)" = python ]; then echo "env PYTHONPATH=/opt/ruletaker/python /opt/ruletaker/python/venv/bin/python -m ruletaker"; else echo /opt/ruletaker/bin/ruletaker; fi'


remind_env() {  # the settings file left in this shell holds the private key or another secret: said once it is on the VM
  [ -n "${ENV_UPLOADED:-}" ] && [ -f "$ENV_UPLOADED" ] || return 0
  local what=""
  if grep -qE '^[[:space:]]*(export[[:space:]]+)?PREDICT_PRIVATE_KEY[[:space:]]*=[[:space:]]*["'"'"']?(0x)?[0-9a-fA-F]{64}' "$ENV_UPLOADED"; then
    what="私钥"
  fi
  if grep -qE '^[[:space:]]*(export[[:space:]]+)?(PREDICT_API_KEY|RULE_API_KEY(_[2-9])?|TELEGRAM_BOT_TOKEN|TELEGRAM_COMMAND_TOKEN|STATUS_KEY|BSC_RPC_URL)[[:space:]]*=[[:space:]]*[^[:space:]#]' "$ENV_UPLOADED"; then
    what="${what:+$what、}API Key / 令牌"
  fi
  if [ -n "$what" ]; then
    echo "提醒：$ENV_UPLOADED 还留在这里（里面有${what}）。虚拟机上已经加密保存了一份，删掉这里的：shred -u \"$ENV_UPLOADED\""
    echo "      以后改设置直接在虚拟机上改：bash deploy/gcp/rt.sh edit 或 rt.sh set 名字；换私钥：bash deploy/gcp/rt.sh key（都不用在这里放）"
  fi
}

vm_call() {  # vm_call WORDS...: deploy/gcp/vm-call.sh run on the VM as root (the service's own pages, at 127.0.0.1). The words
  # are fixed ones of this script (a mode is checked against off|paper|on first) and quoted again for the remote shell.
  ssh_to "$VM" "$ZONE" --command "sudo bash -s -- $(printf '%q ' "$@")" < "$ROOT/deploy/gcp/vm-call.sh"
}

cmd="${1:-help}"
shift || true
# gcloud signed in first: without it every SSH to the VM fails, and what fails after reads like another problem (an upload cut
# short, a command "not there yet"). Cloud Shell in Google's phone app often starts that way.
case "$cmd" in
  help|-h|--help) ;;
  *)
    if [ -z "$(gcloud auth list --filter=status:ACTIVE --format='value(account)' 2>/dev/null)" ]; then
      die "gcloud 还没登录（手机 App 里的 Cloud Shell 常这样），连不上虚拟机：先运行 gcloud auth login——点它给的链接，用你的 Google 账号登录，把页面上的验证码粘贴回来按回车，再重跑这条命令"
    fi
    ;;
esac
case "$cmd" in
  help|-h|--help|probe) ;;
  *) iap_mode ;;
esac

vm_has() {  # vm_has TEST: yes / no on the VM; dies, saying so, when the VM cannot be reached (gcloud's own words above it)
  local said
  said="$(ssh_to "$VM" "$ZONE" --command "if $1; then echo yes; else echo no; fi" | tr -d '[:space:]' || true)"
  case "$said" in
    yes) return 0 ;;
    no) return 1 ;;
    *) die "连不上虚拟机 $VM（原因见上面 gcloud / SSH 的报错），什么都没改" ;;
  esac
}
case "$cmd" in
  deploy)
    if gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" >/dev/null 2>&1; then
      echo "虚拟机 $VM 已经在 $ZONE：直接更新"
    else
      echo "在 $ZONE（东京）创建虚拟机 $VM（$MACHINE）…"
      create_vm "$VM" "$ZONE" "$MACHINE" 20GB "$TAG"
      wait_ssh "$VM" "$ZONE"
    fi
    upload_code "$VM" "$ZONE"
    upload_env "${1:-}"
    setup
    echo "部署完成。看状态：bash deploy/gcp/rt.sh status；看日志：bash deploy/gcp/rt.sh logs"
    [ ${#IAP[@]} -gt 0 ] || echo "建议接着运行一次安全加固：bash deploy/gcp/rt.sh lockdown（封掉所有入站连接，SSH 只走 Google 登录的 IAP 通道）"
    remind_env
    ;;
  lockdown)
    # Nothing reaches the VM from the internet afterwards: the service only ever connects out (Predict, the chain's node,
    # Binance, Telegram), its pages listen on 127.0.0.1, and SSH comes through IAP — Google checks the login and the project's
    # permissions before a single packet gets to the machine. The order below never locks anyone out: the tunnel is proven
    # to work before the door is shut, and the shut door is opened again should the proof fail afterwards.
    gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" >/dev/null 2>&1 || die "还没有虚拟机 $VM：先 bash deploy/gcp/rt.sh deploy"
    net="$(gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" --format='value(networkInterfaces[0].network.basename())' 2>/dev/null || true)"
    net="${net:-default}"
    echo "1/5 允许 Google 的 IAP 通道连这台机器的 SSH（网络 $net）…"
    gcloud services enable iap.googleapis.com "${G[@]}" >/dev/null 2>&1 || true
    gcloud compute firewall-rules describe "$RULE_IAP" "${G[@]}" >/dev/null 2>&1 || \
      gcloud compute firewall-rules create "$RULE_IAP" "${G[@]}" --network "$net" --direction INGRESS --action ALLOW --rules tcp:22 \
        --source-ranges "$IAP_RANGE" --target-tags "$TAG" --priority 900 --description "ruletaker: SSH through the IAP tunnel only" >/dev/null
    gcloud compute instances add-tags "$VM" "${G[@]}" "${Z[@]}" --tags "$TAG" >/dev/null
    echo "2/5 试一下通过 IAP 通道能不能登录（还没封任何东西）…"
    tunnel_ok() {
      for _ in 1 2 3 4 5 6 7 8; do
        if gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" --quiet --tunnel-through-iap --command true >/dev/null 2>&1; then
          return 0
        fi
        sleep 5
      done
      return 1
    }
    tunnel_ok || die "通过 IAP 通道登录不了，所以什么都没有封（现在和加固前一样）。常见原因：账号不是项目所有者（要有 IAP-secured Tunnel User 角色），或 IAP API 没启用。手动试：gcloud compute ssh $VM --zone $ZONE --tunnel-through-iap"
    echo "3/5 封掉其他所有入站连接…"
    gcloud compute firewall-rules describe "$RULE_DENY" "${G[@]}" >/dev/null 2>&1 || \
      gcloud compute firewall-rules create "$RULE_DENY" "${G[@]}" --network "$net" --direction INGRESS --action DENY --rules all \
        --source-ranges 0.0.0.0/0 --target-tags "$TAG" --priority 1000 --description "ruletaker: nothing else gets in from outside" >/dev/null
    echo "4/5 封好之后再登录一次确认…"
    if ! tunnel_ok; then
      gcloud compute firewall-rules delete "$RULE_DENY" "${G[@]}" --quiet >/dev/null 2>&1 || true
      die "封好之后通过 IAP 登录不了，已经把刚才那条封锁规则撤掉（现在和加固前一样）"
    fi
    # The second layer: this machine's own firewall (vm-firewall.sh), the same door — so a firewall rule of the project's
    # deleted or changed later leaves the machine as shut. Put on with its own undo already set (3 minutes), kept only once a
    # fresh login through the tunnel proves it opens: nobody is locked out, whatever goes wrong.
    echo "5/5 机器自己的防火墙（第二道：就算项目的防火墙规则被删了，机器也只放行 IAP 通道的 SSH）…"
    if vm_has "test -f /opt/ruletaker/deploy/gcp/vm-firewall.sh"; then
      if remote "sudo bash /opt/ruletaker/deploy/gcp/vm-firewall.sh arm"; then
        if tunnel_ok; then
          remote "sudo bash /opt/ruletaker/deploy/gcp/vm-firewall.sh confirm" || echo "（机器防火墙没确认上：3 分钟内自动撤销，Google Cloud 那层照旧）"
        else
          echo "⚠️ 加上机器防火墙后通过 IAP 登录不了：它 3 分钟内会自动撤销（什么都不用做），Google Cloud 那层照旧"
        fi
      else
        echo "（机器防火墙没加，原因见上；Google Cloud 那层照旧）"
      fi
    else
      echo "（虚拟机上的版本还没有机器防火墙：先 bash deploy/gcp/rt.sh update，再运行一次 bash deploy/gcp/rt.sh lockdown）"
    fi
    echo "✅ 加固完成：这台机器不再接受任何来自公网的连接；rt.sh 的所有命令从现在起自动走 IAP 通道（会慢一两秒）。"
    echo "   解除：bash deploy/gcp/rt.sh unlock"
    ;;
  nosa)
    # A VM made before the script created it without one carries the project's default service account (Editor on the whole
    # project unless that was changed; what the machine may do with it is cut by its scopes): taken off. Only a stopped machine's
    # can be changed — the service withdraws what rests on the book as it stops and starts again with the machine.
    sa="$(gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" --format='value(serviceAccounts[].email)' 2>/dev/null || true)"
    if [ -z "$sa" ]; then
      echo "✅ 虚拟机 $VM 本来就不带服务账号"
      exit 0
    fi
    echo "虚拟机 $VM 带着服务账号 $sa。去掉它要先停机：服务会先撤掉挂着的单，约 1 分钟后随机器重新启动。"
    echo "不要在抢单前后几分钟做（对手挂单时刻、开场前后）。"
    read -r -p "现在停机去掉服务账号？输入 yes 确认：" ok
    [ "$ok" = "yes" ] || die "没有改动"
    gcloud compute instances stop "$VM" "${G[@]}" "${Z[@]}"
    changed=0
    if gcloud compute instances set-service-account "$VM" "${G[@]}" "${Z[@]}" --no-service-account --no-scopes; then
      changed=1
    fi
    gcloud compute instances start "$VM" "${G[@]}" "${Z[@]}"   # (started again whatever happened above)
    wait_ssh "$VM" "$ZONE"
    remote "sudo systemctl is-active --quiet ruletaker && echo '✅ 服务已经重新运行' || echo '⚠️ 服务没起来：bash deploy/gcp/rt.sh logs 看原因'"
    if [ "$changed" = 1 ]; then echo "✅ 服务账号已去掉：进了这台机器也拿不到操作项目的令牌"; else die "服务账号没去掉（上面有 gcloud 的报错），机器已重新启动"; fi
    ;;
  unlock)
    # the machine's own firewall first, while the tunnel still opens (afterwards SSH comes from anywhere, which it would refuse)
    if [ ${#IAP[@]} -gt 0 ] && vm_has "test -f /opt/ruletaker/deploy/gcp/vm-firewall.sh"; then
      remote "sudo bash /opt/ruletaker/deploy/gcp/vm-firewall.sh remove" || die "机器自己的防火墙没拿掉（原因见上），所以什么都没解除"
    fi
    gcloud compute firewall-rules delete "$RULE_DENY" "${G[@]}" --quiet 2>/dev/null || echo "（本来就没有封锁规则）"
    echo "已解除加固：SSH 端口恢复成 Google Cloud 默认的对外可连（仍然只认密钥、不认密码）。再加固：bash deploy/gcp/rt.sh lockdown"
    ;;
  update)
    upload_code "$VM" "$ZONE"
    setup
    ;;
  edit)
    # The settings file edited where it lives (root's, on the VM): the key need not be kept in Cloud Shell to change a setting.
    # Afterwards (vm-key.sh apply): a key — or an API key, a token — pasted into it kept encrypted and taken out again, the
    # settings checked the way the service reads them, and only then the restart — a setting that does not parse leaves the
    # running service as it is.
    # (A VM still on the version before: checked and restarted as before.)
    ssh_to "$VM" "$ZONE" --command "sudo nano /etc/ruletaker/ruletaker.env; \
      if [ -f /opt/ruletaker/deploy/gcp/vm-key.sh ]; then sudo bash /opt/ruletaker/deploy/gcp/vm-key.sh apply; \
      elif sudo -u ruletaker \$($RUN_ON_VM) plan --env /etc/ruletaker/ruletaker.env >/dev/null; then \
        sudo systemctl restart ruletaker && echo '已保存，服务已重启'; \
      else echo '!! 设置文件有误（见上面），服务没有重启、还在按原来的设置运行：再运行 bash deploy/gcp/rt.sh edit 改正'; fi" -- -t
    ;;
  set)
    # One setting changed from anywhere — a phone too (nano wants a Ctrl key a phone has not): every line of it replaced on the
    # VM, checked the way the service reads it, the service restarted; a value the service refuses puts the file back as it was
    # (vm-key.sh put). No value given: asked for without being shown (a key) — either way carried over SSH's standard input,
    # never on a command line.
    name="${1:-}"
    [[ "$name" =~ ^[A-Z][A-Z0-9_]*$ ]] \
      || die "用法：rt.sh set 设置名 值（例：rt.sh set RULE_WS_CONNECTIONS 3）；不写值就隐藏输入（填 Key 用：rt.sh set RULE_API_KEY_2）"
    [ "$name" != PREDICT_PRIVATE_KEY ] || die "私钥用 bash deploy/gcp/rt.sh key 换（会加密保存，不放进设置文件）"
    vm_has "grep -q '^  put)' /opt/ruletaker/deploy/gcp/vm-key.sh" \
      || die "虚拟机上的版本还没有这个命令：先运行 bash deploy/gcp/rt.sh update"
    if [[ "$name" =~ ^(PREDICT_API_KEY|RULE_API_KEY(_[2-9])?|TELEGRAM_BOT_TOKEN|TELEGRAM_COMMAND_TOKEN|STATUS_KEY|BSC_RPC_URL)$ ]]; then
      # (a secret: kept encrypted on the VM — never written into the settings file there by an older version)
      vm_has "grep -q ruletaker_secrets /opt/ruletaker/deploy/gcp/vm-key.sh" \
        || die "虚拟机上的版本还不会加密保存 $name：先运行 bash deploy/gcp/rt.sh update，再设它（什么都没改）"
    fi
    if [ $# -ge 2 ]; then
      value="$2"
    else
      value=""
      IFS= read -r -s -p "$name 的值（屏幕上不显示）：" value || true
      echo
    fi
    printf '%s\n' "$value" | ssh_to "$VM" "$ZONE" --command "sudo bash /opt/ruletaker/deploy/gcp/vm-key.sh put $name"
    value=""
    ;;
  key)
    # A new private key: pasted here without being shown (not echoed, not in the shell's history, never on a command line),
    # carried over the SSH channel's standard input to vm-key.sh on the VM — which checks it (its address, and on chain whether
    # it controls PREDICT_ACCOUNT) before anything changes, keeps it encrypted, takes any plain copy out of the settings file
    # and restarts the service on it.
    vm_has "test -f /opt/ruletaker/deploy/gcp/vm-key.sh" \
      || die "虚拟机上的版本还不会加密保存私钥：先运行 bash deploy/gcp/rt.sh update（它会顺便把设置文件里的私钥收进加密保存）"
    echo "换私钥：粘贴新的私钥（64 位十六进制，0x 可省）后按回车。屏幕上不会显示，也不会进命令历史。"
    echo "（先核对它在链上是不是 PREDICT_ACCOUNT 的控制钥匙，不对就什么都不改；对了才加密保存、重启服务——别在抢单前后几分钟做）"
    newkey=""
    IFS= read -r -s -p "新私钥：" newkey || true
    echo
    newkey="${newkey//[[:space:]]/}"
    [[ "$newkey" =~ ^(0x)?[0-9a-fA-F]{64}$ ]] || { newkey=""; die "这不是 64 位十六进制的私钥：什么都没改"; }
    printf '%s\n' "$newkey" | ssh_to "$VM" "$ZONE" --command "sudo bash /opt/ruletaker/deploy/gcp/vm-key.sh set"
    newkey=""
    ;;
  audit)
    # Read-only: what holds the key, who can read it, what reaches the machine. The key is never shown, here or there.
    echo "== 私钥安全体检（只读，什么都不改；私钥本身不会显示）=="
    echo "-- Cloud Shell 这边 --"
    left="$(grep -lsE '^[[:space:]]*(export[[:space:]]+)?PREDICT_PRIVATE_KEY[[:space:]]*=[[:space:]]*[\"'"'"']?[0-9a-fA-Fx]{64}' \
      "$ROOT"/*.env "$ROOT"/.env "$ROOT"/python/*.env "$HOME"/*.env 2>/dev/null || true)"
    if [ -n "$left" ]; then
      echo "⚠️ 这些文件里还有私钥：$(echo $left)——虚拟机上已有一份，删掉：shred -u 文件名（以后改设置用 rt.sh edit）"
    else
      echo "✅ Cloud Shell 里没有留着私钥的设置文件"
    fi
    if grep -qsiE 'PRIVATE_KEY[[:space:]]*=[[:space:]]*[\"'"'"']?(0x)?[0-9a-f]{64}' "$HOME/.bash_history"; then
      echo "⚠️ 命令历史 ~/.bash_history 里有私钥（在命令行里打过）：history -c && shred -u ~/.bash_history"
    else
      echo "✅ 命令历史里没有私钥"
    fi
    echo "-- Google Cloud 项目 $PROJECT --"
    sa="$(gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" --format='value(serviceAccounts[].email)' 2>/dev/null || true)"
    if [ -z "$sa" ]; then echo "✅ 虚拟机不带服务账号（进了机器也拿不到操作项目的令牌）"; else echo "⚠️ 虚拟机带着服务账号 $sa：去掉它运行 bash deploy/gcp/rt.sh nosa（停机约 1 分钟）"; fi
    if [ ${#IAP[@]} -gt 0 ]; then
      echo "✅ 已加固（lockdown）：公网连不进这台机器，SSH 只走 Google 登录的 IAP 通道"
    else
      echo "⚠️ 还没加固：运行 bash deploy/gcp/rt.sh lockdown（封掉所有入站连接）"
    fi
    who="$(gcloud projects get-iam-policy "$PROJECT" --flatten='bindings[].members' --format='value(bindings.members)' 2>/dev/null \
      | grep -E '^(user|group|domain):|allUsers|allAuthenticatedUsers' | sort -u | tr '\n' ' ' || true)"
    echo "项目里有权限的人（都能 SSH 进机器读到私钥，应该只有你自己）：${who:-（读不到：账号没有查看权限）}"
    ssh_to "$VM" "$ZONE" --command "sudo bash -s" < "$ROOT/deploy/gcp/vm-audit.sh"
    ;;
  env)
    upload_env "${1:-}"
    setup
    remind_env
    ;;
  console)
    # The page on the VM listens on 127.0.0.1 only: an SSH tunnel brings it to this Cloud Shell's port, and Cloud Shell's web
    # preview shows that port in the browser, behind the Google login — no port is opened to the internet.
    LOCAL="${1:-8080}"
    # The page's port as the settings file has it. A failed look (an SSH hiccup through IAP) never ends the command unsaid:
    # told, and 8080 (the default) taken.
    echo "连接虚拟机（${IAP[*]:+走 Google 的 IAP 通道，}要几秒）…"
    err="$(mktemp)"
    REMOTE_PORT="$(vm_call port 2>"$err" | tr -d '[:space:]' || true)"
    if [ -z "$REMOTE_PORT" ] && grep -qviE 'numpy|increasing_the_tcp|^$|^warning:|To increase the performance|please see|ctions,' "$err"; then
      echo "⚠️ 读设置时 SSH 报错（下面是原话），先按默认端口 8080 接通道试试："
      grep -viE 'numpy|increasing_the_tcp|To increase the performance|please see|ctions,|^warning:$|^$' "$err" | tail -5 | sed 's/^/   /'
    fi
    rm -f "$err"
    case "$REMOTE_PORT" in ''|*[!0-9]*) REMOTE_PORT=8080 ;; esac
    tunnel=""
    trap 'kill_tree "$tunnel"; echo; echo "通道已关闭"; exit 0' INT TERM
    open_tunnel() {  # the tunnel up and the page answering through it: 0; why not: 1 (said)
      ssh_to "$VM" "$ZONE" -- -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
        -L "$LOCAL:127.0.0.1:$REMOTE_PORT" &
      tunnel=$!
      # Told ready only once the page itself answers through it (the service's /healthz: no key needed)
      local state="down" silent=0 code
      for _ in $(seq 1 60); do
        kill -0 "$tunnel" 2>/dev/null || { state="gone"; break; }
        code=0
        curl -s -o /dev/null --max-time 3 "http://127.0.0.1:$LOCAL/healthz" || code=$?
        if [ "$code" = 0 ]; then state="up"; break; fi
        if [ "$code" = 52 ] || [ "$code" = 56 ]; then  # (the tunnel is up, nothing answers behind it: a few tries, a restart's room)
          silent=$((silent + 1))
          if [ "$silent" -ge 10 ]; then state="noservice"; break; fi
        fi
        sleep 1
      done
      case "$state" in
        up) return 0 ;;
        noservice) kill_tree "$tunnel"; echo "⚠️ 通道接通了，但虚拟机上的服务没有回应（bash deploy/gcp/rt.sh status 或 logs 看看）" ;;
        gone) echo "⚠️ 通道没接通（原因见上面 gcloud / ssh 的报错；端口 $LOCAL 被占用的话换一个：bash deploy/gcp/rt.sh console 8081）" ;;
        *) kill_tree "$tunnel"; echo "⚠️ 60 秒内通道没接通（原因见上面的报错）" ;;
      esac
      return 1
    }
    echo "接通控制台通道：Cloud Shell 端口 $LOCAL → 虚拟机 $VM 的 127.0.0.1:$REMOTE_PORT（${IAP[*]:+走 Google 的 IAP 通道，}要几秒，先别点网页预览）…"
    first=1
    fails=0
    while true; do
      if open_tunnel; then
        fails=0
        if [ "$first" = 1 ]; then
          first=0
          echo "✅ 通道已接通。现在点 Cloud Shell 工具栏右上角的「网页预览」（Web preview，方框里一个眼睛的图标）→「在端口 $LOCAL 上预览」，浏览器会打开控制台和复盘页。"
          echo "这个窗口保持打开，看完按 Ctrl+C 关掉通道（要运行别的命令，点终端标签旁边的 + 开一个新的）。断了会自动重连。"
          echo "（这个页面跟着 Cloud Shell 走：Cloud Shell 闲置太久会被 Google 收回，通道也就没了，回来再运行一次这个命令。随时能用的是 Telegram 机器人的 /menu。）"
        else
          echo "✅ $(date +%H:%M:%S) 已重新接通，刷新网页就好"
        fi
        wait "$tunnel" || true
        echo "⚠️ $(date +%H:%M:%S) 通道断了，自动重连…"
      else
        fails=$((fails + 1))
        if [ "$fails" -ge 5 ]; then
          die "连续 $fails 次接不通，先停下：bash deploy/gcp/rt.sh status 看服务在不在运行，或稍后再试"
        fi
        echo "   $((fails * 5)) 秒后再试（第 $fails 次）…"
      fi
      sleep $((fails * 5 + 2))
    done
    ;;
  status) vm_call get /status ;;
  json) vm_call get /status.json ;;
  pause) vm_call post /pause ;;
  resume) vm_call post /resume ;;
  mode)
    case "${1:-}" in
      off|paper|on) vm_call post "/mode?to=$1" ;;
      *) die "用法：rt.sh mode off|paper|on" ;;
    esac
    ;;
  logs) remote "sudo journalctl -u ruletaker -f -n 200 --no-hostname -o cat" ;;
  check|test)  # the running service's own self-check (or order test); the command-line check when the service is not running
    vm_call control "$cmd"
    ;;
  latency)
    # (RULE_TAKER=off: a measurement needs no private key, and is not handed one — the API keys kept encrypted are opened for
    # it, as for the service: vm-key.sh measure. A rate given here is this measurement's alone — the REST reads use a quarter
    # of it — and the service's own settings stay as they are. A VM still on the version before: as before.)
    rate="${1:-}"
    case "$rate" in
      '') ;;
      *[!0-9]*) die "用法：rt.sh latency [每分钟请求数]（只给这次测量用，例：4000；不写就按设置文件里的 PREDICT_RATE_PER_MIN）" ;;
    esac
    remote "cd / && if [ -f /opt/ruletaker/deploy/gcp/vm-key.sh ] && grep -q '^  measure)' /opt/ruletaker/deploy/gcp/vm-key.sh; then \
      sudo bash /opt/ruletaker/deploy/gcp/vm-key.sh measure $rate; \
      else sudo -u ruletaker env RULE_TAKER=off ${rate:+PREDICT_RATE_PER_MIN=$rate }\$($RUN_ON_VM) latency --env /etc/ruletaker/ruletaker.env; fi"
    ;;
  restart|stop|start) remote "sudo systemctl $cmd ruletaker && sudo systemctl --no-pager --lines=5 status ruletaker || true" ;;
  ssh) ssh_to "$VM" "$ZONE" ;;
  probe)
    zone="${1:-}"
    [ -n "$zone" ] || die "用法：rt.sh probe 区域（例：asia-northeast1-b 东京、asia-east2-a 香港、asia-northeast3-a 首尔、asia-southeast1-a 新加坡）"
    file="${2:-}"
    # The API key alone goes to the throwaway machine — never the private key, the Telegram tokens or anything else. Read on the
    # service's own VM as the service reads it, and passed straight on (never on disk here); or from a file named here.
    if [ -n "$file" ]; then
      [ -f "$file" ] || die "找不到 $file（要用里面的 API Key 读 Predict）"
      keys="$(grep -E '^(RULE_API_KEY|PREDICT_API_KEY|PREDICT_CHAIN_ID|PREDICT_RATE_PER_MIN)=' "$file" || true)"
    else
      iap_mode
      echo "从虚拟机 $VM 的设置文件读 API Key（只读这一项，不读私钥）…"
      keys="$(vm_call probe-keys || true)"
    fi
    printf '%s\n' "$keys" | grep -qE '^(RULE_API_KEY|PREDICT_API_KEY)=.+' || die "${file:-虚拟机 $VM 的设置文件}里没有填 RULE_API_KEY / PREDICT_API_KEY"
    name="rt-probe-$(date +%s)"
    echo "在 $zone 临时创建 $name 量延迟（量完删除；只带 API Key 过去，不带私钥；量网络用不着编译，临时机器上用轻量的 Python 版）…"
    create_vm "$name" "$zone" "${PROBE_MACHINE:-$MACHINE}" 10GB  # (the same kind of machine as the service's: like with like)
    trap 'echo "删除临时虚拟机 $name…"; gcloud compute instances delete "$name" "${G[@]}" --zone "$zone" --quiet >/dev/null 2>&1 || true' EXIT
    wait_ssh "$name" "$zone"
    upload_code "$name" "$zone"
    printf '%s\n' "$keys" | ssh_to "$name" "$zone" --command "umask 077; cat > /tmp/probe.env"
    ssh_to "$name" "$zone" --command \
      "sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300 update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300 install -y -qq python3-venv >/dev/null && cd /opt/ruletaker/python && sudo python3 -m venv venv && sudo venv/bin/pip install -q --require-hashes --only-binary :all: -r requirements.lock && RULE_TAKER=off venv/bin/python -m ruletaker latency --env /tmp/probe.env"
    ;;
  delete)
    read -r -p "确定删除虚拟机 $VM（$ZONE）？上面的记录和设置会一起删掉。输入 yes 确认：" ok
    [ "$ok" = "yes" ] && gcloud compute instances delete "$VM" "${G[@]}" "${Z[@]}" --quiet
    ;;
  *)
    awk 'NR > 1 { if (!/^#/) exit; print }' "$0"   # (the comment at the top: the commands)
    ;;
esac
