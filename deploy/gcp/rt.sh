#!/usr/bin/env bash
# 规则吃单 · Google Cloud 部署和日常操作（在 Google Cloud Shell 里、仓库根目录下运行）
#
#   bash deploy/gcp/rt.sh deploy          第一次：在东京建一台虚拟机（asia-northeast1），装好、启动
#   bash deploy/gcp/rt.sh lockdown        安全加固：封掉这台机器所有对外的入站连接，SSH 只能走 Google 的 IAP 通道（要 Google 登录）
#   bash deploy/gcp/rt.sh unlock          解除上面的加固
#   bash deploy/gcp/rt.sh update          代码更新后：重新上传代码、编译、跑测试、重启（设置不动）
#   RT_IMPL=python bash deploy/gcp/rt.sh update   换回 Python 版（备用）；RT_IMPL=rust 再换回 Rust 版（记录、设置都通用）
#   bash deploy/gcp/rt.sh env [文件]      上传新的设置文件（默认 ./ruletaker.env）并重启
#   bash deploy/gcp/rt.sh console         打开网页控制台和复盘页（通过 Cloud Shell 的“网页预览”，不开公网端口）
#   bash deploy/gcp/rt.sh status          看状态（就是 huice 的 /rules）
#   bash deploy/gcp/rt.sh logs            看实时日志（Ctrl+C 退出）
#   bash deploy/gcp/rt.sh check           自检：登录、账户、余额、授权、下单接口、推送、记录（和 Telegram /check 一样）
#   bash deploy/gcp/rt.sh test            挂单测试：挂一张约 $1 的低价单、查到、撤掉（和 Telegram /test 一样）
#   bash deploy/gcp/rt.sh latency         量这台机器到 Predict 的延迟（只读）
#   bash deploy/gcp/rt.sh pause|resume    暂停 / 恢复开新单
#   bash deploy/gcp/rt.sh mode off|paper|on   运行中切换模式（不会高于设置文件里的 RULE_TAKER）
#   bash deploy/gcp/rt.sh restart|stop|start|ssh
#   bash deploy/gcp/rt.sh probe ZONE      在别的区域临时开一台小机器量延迟，量完自动删除（例：asia-east2-a；只带 API Key，不带私钥）
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
}

# The command line of the version in use on the VM (its subcommands: run, check, latency, plan).
RUN_ON_VM='if [ "$(cat /opt/ruletaker/IMPL 2>/dev/null || echo rust)" = python ]; then echo "env PYTHONPATH=/opt/ruletaker/python /opt/ruletaker/python/venv/bin/python -m ruletaker"; else echo /opt/ruletaker/bin/ruletaker; fi'


remind_env() {  # the settings file left in this shell holds the private key: said once it is on the VM
  if [ -n "${ENV_UPLOADED:-}" ] && [ -f "$ENV_UPLOADED" ]; then
    echo "提醒：$ENV_UPLOADED 还留在这里（里面有私钥）。虚拟机上已经有一份，这里不再需要的话删掉：shred -u \"$ENV_UPLOADED\""
  fi
}

status_call() {  # GET or POST on the service's own status page, on the VM (127.0.0.1)
  local method="$1" path="$2"
  remote "sudo bash -c 'F=/etc/ruletaker/ruletaker.env; K=\$(grep -E \"^STATUS_KEY=\" \$F | tail -1 | cut -d= -f2-); P=\$(grep -E \"^STATUS_PORT=\" \$F | tail -1 | cut -d= -f2-); curl -s -X $method -H \"X-RT: 1\" -H \"X-Status-Key: \$K\" \"http://127.0.0.1:\${P:-8080}$path\"; echo'"
}

cmd="${1:-help}"
shift || true
case "$cmd" in
  help|-h|--help|probe) ;;
  *) iap_mode ;;
esac
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
    echo "1/4 允许 Google 的 IAP 通道连这台机器的 SSH（网络 $net）…"
    gcloud services enable iap.googleapis.com "${G[@]}" >/dev/null 2>&1 || true
    gcloud compute firewall-rules describe "$RULE_IAP" "${G[@]}" >/dev/null 2>&1 || \
      gcloud compute firewall-rules create "$RULE_IAP" "${G[@]}" --network "$net" --direction INGRESS --action ALLOW --rules tcp:22 \
        --source-ranges "$IAP_RANGE" --target-tags "$TAG" --priority 900 --description "ruletaker: SSH through the IAP tunnel only" >/dev/null
    gcloud compute instances add-tags "$VM" "${G[@]}" "${Z[@]}" --tags "$TAG" >/dev/null
    echo "2/4 试一下通过 IAP 通道能不能登录（还没封任何东西）…"
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
    echo "3/4 封掉其他所有入站连接…"
    gcloud compute firewall-rules describe "$RULE_DENY" "${G[@]}" >/dev/null 2>&1 || \
      gcloud compute firewall-rules create "$RULE_DENY" "${G[@]}" --network "$net" --direction INGRESS --action DENY --rules all \
        --source-ranges 0.0.0.0/0 --target-tags "$TAG" --priority 1000 --description "ruletaker: nothing else gets in from outside" >/dev/null
    echo "4/4 封好之后再登录一次确认…"
    if ! tunnel_ok; then
      gcloud compute firewall-rules delete "$RULE_DENY" "${G[@]}" --quiet >/dev/null 2>&1 || true
      die "封好之后通过 IAP 登录不了，已经把刚才那条封锁规则撤掉（现在和加固前一样）"
    fi
    echo "✅ 加固完成：这台机器不再接受任何来自公网的连接；rt.sh 的所有命令从现在起自动走 IAP 通道（会慢一两秒）。"
    echo "   解除：bash deploy/gcp/rt.sh unlock"
    ;;
  unlock)
    gcloud compute firewall-rules delete "$RULE_DENY" "${G[@]}" --quiet 2>/dev/null || echo "（本来就没有封锁规则）"
    echo "已解除加固：SSH 端口恢复成 Google Cloud 默认的对外可连（仍然只认密钥、不认密码）。再加固：bash deploy/gcp/rt.sh lockdown"
    ;;
  update)
    upload_code "$VM" "$ZONE"
    setup
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
    REMOTE_PORT="$(ssh_to "$VM" "$ZONE" --command "sudo grep -E '^STATUS_PORT=' /etc/ruletaker/ruletaker.env | tail -1 | cut -d= -f2-" 2>/dev/null | tr -d '[:space:]')"
    REMOTE_PORT="${REMOTE_PORT:-8080}"
    echo "控制台通道：Cloud Shell 端口 $LOCAL → 虚拟机 $VM 的 127.0.0.1:$REMOTE_PORT"
    echo "现在点 Cloud Shell 工具栏右上角的「网页预览」（方框里一个眼睛的图标）→「在端口 $LOCAL 上预览」，浏览器会打开控制台和复盘页。"
    echo "（用自己电脑上的 gcloud 也行：gcloud compute ssh $VM --zone $ZONE ${IAP[*]+${IAP[*]} }-- -N -L $LOCAL:127.0.0.1:$REMOTE_PORT，然后打开 http://localhost:$LOCAL）"
    echo "这个窗口保持打开，看完按 Ctrl+C 关掉通道。"
    ssh_to "$VM" "$ZONE" -- -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -L "$LOCAL:127.0.0.1:$REMOTE_PORT"
    ;;
  status) status_call GET /status ;;
  json) status_call GET /status.json ;;
  pause) status_call POST /pause ;;
  resume) status_call POST /resume ;;
  mode) [ -n "${1:-}" ] || die "用法：rt.sh mode off|paper|on"; status_call POST "/mode?to=$1" ;;
  logs) remote "sudo journalctl -u ruletaker -f -n 200 --no-hostname -o cat" ;;
  check|test)  # the running service's own self-check (or order test); the command-line check when the service is not running
    remote "sudo bash -c 'F=/etc/ruletaker/ruletaker.env; K=\$(grep -E \"^STATUS_KEY=\" \$F | tail -1 | cut -d= -f2-); P=\$(grep -E \"^STATUS_PORT=\" \$F | tail -1 | cut -d= -f2-); \
      curl -sf -m 90 -X POST -H \"X-RT: 1\" -H \"X-Status-Key: \$K\" -H \"Content-Type: application/json\" -d \"{\\\"action\\\":\\\"$cmd\\\"}\" \"http://127.0.0.1:\${P:-8080}/api/control\" \
      | python3 -c \"import json,sys; print(json.load(sys.stdin)[\\\"message\\\"])\" \
      || { [ $cmd = check ] && echo \"（服务没在运行：改用命令行自检）\" && sudo -u ruletaker \$($RUN_ON_VM) check --env \$F; }'"
    ;;
  latency) remote "sudo -u ruletaker \$($RUN_ON_VM) latency --env /etc/ruletaker/ruletaker.env" ;;
  restart|stop|start) remote "sudo systemctl $cmd ruletaker && sudo systemctl --no-pager --lines=5 status ruletaker || true" ;;
  ssh) ssh_to "$VM" "$ZONE" ;;
  probe)
    zone="${1:-}"
    [ -n "$zone" ] || die "用法：rt.sh probe 区域（例：asia-northeast1-b 东京、asia-east2-a 香港、asia-northeast3-a 首尔、asia-southeast1-a 新加坡）"
    file="${2:-$ROOT/ruletaker.env}"
    [ -f "$file" ] || die "找不到 $file（要用里面的 API Key 读 Predict）"
    # The API key alone goes to the throwaway machine — never the private key, the Telegram tokens or anything else of the file.
    keys="$(grep -E '^(RULE_API_KEY|PREDICT_API_KEY|PREDICT_CHAIN_ID)=' "$file" || true)"
    printf '%s\n' "$keys" | grep -qE '^(RULE_API_KEY|PREDICT_API_KEY)=.+' || die "$file 里没有填 RULE_API_KEY / PREDICT_API_KEY"
    name="rt-probe-$(date +%s)"
    echo "在 $zone 临时创建 $name 量延迟（量完删除；只带 API Key 过去，不带私钥；量网络用不着编译，临时机器上用轻量的 Python 版）…"
    create_vm "$name" "$zone" e2-small 10GB
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
    sed -n '2,25p' "$0"
    ;;
esac
