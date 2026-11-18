#!/usr/bin/env bash
# 规则吃单 · Google Cloud 部署和日常操作（在 Google Cloud Shell 里、仓库根目录下运行）
#
#   bash deploy/gcp/rt.sh deploy          第一次：在东京建一台虚拟机（asia-northeast1），装好、启动
#   bash deploy/gcp/rt.sh update          代码更新后：重新上传代码、跑测试、重启（设置不动）
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
#   bash deploy/gcp/rt.sh probe ZONE      在别的区域临时开一台小机器量延迟，量完自动删除（例：asia-east2）
#   bash deploy/gcp/rt.sh delete          删除虚拟机（数据一起删掉，先确认）
#
# 可用环境变量改默认值：PROJECT（默认 gcloud 当前项目）ZONE（asia-northeast1-b）VM（ruletaker）MACHINE（e2-standard-2）
set -euo pipefail

PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null || true)}"
ZONE="${ZONE:-asia-northeast1-b}"
VM="${VM:-ruletaker}"
MACHINE="${MACHINE:-e2-standard-2}"
IMAGE_FAMILY="${IMAGE_FAMILY:-ubuntu-2404-lts-amd64}"
IMAGE_PROJECT="${IMAGE_PROJECT:-ubuntu-os-cloud}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

die() { echo "错误：$*" >&2; exit 1; }
[ -n "$PROJECT" ] || die "没有选 Google Cloud 项目：先运行 gcloud config set project 你的项目ID"
G=(--project "$PROJECT")
Z=(--zone "$ZONE")

remote() {  # a command on the VM
  gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" --quiet --command "$1"
}

wait_ssh() {
  local name="$1" zone="$2"
  echo "等虚拟机 $name 可以 SSH 登录…"
  for _ in $(seq 1 40); do
    if gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet --command true >/dev/null 2>&1; then
      return 0
    fi
    sleep 6
  done
  die "虚拟机 $name 两分钟内没法 SSH 登录"
}

create_vm() {
  local name="$1" zone="$2" machine="$3" disk="$4"
  gcloud services enable compute.googleapis.com "${G[@]}" >/dev/null
  gcloud compute instances create "$name" "${G[@]}" --zone "$zone" \
    --machine-type "$machine" \
    --image-family "$IMAGE_FAMILY" --image-project "$IMAGE_PROJECT" \
    --boot-disk-size "$disk" --boot-disk-type pd-balanced \
    --network-tier PREMIUM \
    --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring \
    --labels app=ruletaker
}

upload_code() {
  local name="$1" zone="$2"
  echo "上传代码…"
  (cd "$ROOT" && tar czf - --exclude=__pycache__ ruletaker tests requirements.txt deploy/gcp) | gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet \
    --command "sudo mkdir -p /opt/ruletaker && sudo rm -rf /opt/ruletaker/ruletaker /opt/ruletaker/tests /opt/ruletaker/deploy && sudo tar xzf - -C /opt/ruletaker"
}

upload_env() {
  local file="${1:-$ROOT/ruletaker.env}"
  [ -f "$file" ] || die "找不到设置文件 $file：先 cp ruletaker.env.example ruletaker.env 再填好"
  grep -qE '^(RULE_API_KEY|PREDICT_API_KEY)=.+' "$file" || echo "提醒：$file 里 RULE_API_KEY / PREDICT_API_KEY 都是空的，paper / on 模式启动不了"
  echo "上传设置文件 $file（只在虚拟机上保存，权限 640）…"
  gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" --quiet --command "umask 077; cat > /tmp/ruletaker.env" < "$file"
}

setup() {
  remote "sudo bash /opt/ruletaker/deploy/gcp/vm-setup.sh"
}

status_call() {  # GET or POST on the service's own status page, on the VM (127.0.0.1)
  local method="$1" path="$2"
  remote "sudo bash -c 'F=/etc/ruletaker/ruletaker.env; K=\$(grep -E \"^STATUS_KEY=\" \$F | tail -1 | cut -d= -f2-); P=\$(grep -E \"^STATUS_PORT=\" \$F | tail -1 | cut -d= -f2-); curl -s -X $method -H \"X-RT: 1\" -H \"X-Status-Key: \$K\" \"http://127.0.0.1:\${P:-8080}$path\"; echo'"
}

cmd="${1:-help}"
shift || true
case "$cmd" in
  deploy)
    if gcloud compute instances describe "$VM" "${G[@]}" "${Z[@]}" >/dev/null 2>&1; then
      echo "虚拟机 $VM 已经在 $ZONE：直接更新"
    else
      echo "在 $ZONE（东京）创建虚拟机 $VM（$MACHINE）…"
      create_vm "$VM" "$ZONE" "$MACHINE" 20GB
      wait_ssh "$VM" "$ZONE"
    fi
    upload_code "$VM" "$ZONE"
    upload_env "${1:-}"
    setup
    echo "部署完成。看状态：bash deploy/gcp/rt.sh status；看日志：bash deploy/gcp/rt.sh logs"
    ;;
  update)
    upload_code "$VM" "$ZONE"
    setup
    ;;
  env)
    upload_env "${1:-}"
    setup
    ;;
  console)
    # The page on the VM listens on 127.0.0.1 only: an SSH tunnel brings it to this Cloud Shell's port, and Cloud Shell's web
    # preview shows that port in the browser, behind the Google login — no port is opened to the internet.
    LOCAL="${1:-8080}"
    REMOTE_PORT="$(gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" --quiet --command "sudo grep -E '^STATUS_PORT=' /etc/ruletaker/ruletaker.env | tail -1 | cut -d= -f2-" 2>/dev/null | tr -d '[:space:]')"
    REMOTE_PORT="${REMOTE_PORT:-8080}"
    echo "控制台通道：Cloud Shell 端口 $LOCAL → 虚拟机 $VM 的 127.0.0.1:$REMOTE_PORT"
    echo "现在点 Cloud Shell 工具栏右上角的「网页预览」（方框里一个眼睛的图标）→「在端口 $LOCAL 上预览」，浏览器会打开控制台和复盘页。"
    echo "（用自己电脑上的 gcloud 也行：gcloud compute ssh $VM --zone $ZONE -- -N -L $LOCAL:127.0.0.1:$REMOTE_PORT，然后打开 http://localhost:$LOCAL）"
    echo "这个窗口保持打开，看完按 Ctrl+C 关掉通道。"
    gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" --quiet -- -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -L "$LOCAL:127.0.0.1:$REMOTE_PORT"
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
      || { [ $cmd = check ] && echo \"（服务没在运行：改用命令行自检）\" && sudo -u ruletaker /opt/ruletaker/venv/bin/python -m ruletaker check --env \$F; }'"
    ;;
  latency) remote "sudo -u ruletaker /opt/ruletaker/venv/bin/python -m ruletaker latency --env /etc/ruletaker/ruletaker.env" ;;
  restart|stop|start) remote "sudo systemctl $cmd ruletaker && sudo systemctl --no-pager --lines=5 status ruletaker || true" ;;
  ssh) gcloud compute ssh "$VM" "${G[@]}" "${Z[@]}" ;;
  probe)
    zone="${1:-}"
    [ -n "$zone" ] || die "用法：rt.sh probe 区域（例：asia-northeast1-b 东京、asia-east2-a 香港、asia-northeast3-a 首尔、asia-southeast1-a 新加坡）"
    file="${2:-$ROOT/ruletaker.env}"
    [ -f "$file" ] || die "找不到 $file（要用里面的 API Key 读 Predict）"
    name="rt-probe-$(date +%s)"
    echo "在 $zone 临时创建 $name 量延迟（量完删除）…"
    create_vm "$name" "$zone" e2-small 10GB
    trap 'echo "删除临时虚拟机 $name…"; gcloud compute instances delete "$name" "${G[@]}" --zone "$zone" --quiet >/dev/null 2>&1 || true' EXIT
    wait_ssh "$name" "$zone"
    upload_code "$name" "$zone"
    gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet --command "umask 077; cat > /tmp/probe.env" < "$file"
    gcloud compute ssh "$name" "${G[@]}" --zone "$zone" --quiet --command \
      "sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300 update -qq && sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300 install -y -qq python3-venv >/dev/null && cd /opt/ruletaker && sudo python3 -m venv venv && sudo venv/bin/pip install -q -r requirements.txt && RULE_TAKER=off venv/bin/python -m ruletaker latency --env /tmp/probe.env"
    ;;
  delete)
    read -r -p "确定删除虚拟机 $VM（$ZONE）？上面的记录和设置会一起删掉。输入 yes 确认：" ok
    [ "$ok" = "yes" ] && gcloud compute instances delete "$VM" "${G[@]}" "${Z[@]}" --quiet
    ;;
  *)
    sed -n '2,20p' "$0"
    ;;
esac
