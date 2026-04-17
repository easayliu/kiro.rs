#!/usr/bin/env bash
#
# kiro-rs 一键安装脚本（Docker 版本）
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/easayliu/kiro.rs/master/install.sh | bash
#   bash install.sh
#
# 环境变量：
#   INSTALL_DIR   安装目录，默认 ./kiro-rs
#   IMAGE_OWNER   镜像 owner，默认 easayliu
#   IMAGE_TAG     镜像 tag，默认 latest（由 tag 触发的 CI 构建产出）
#   IMAGE_REG     镜像 registry，默认 ghcr.io；国内可用 ghcr.nju.edu.cn
#   PORT          宿主机监听端口，默认 8990
#   API_KEY       客户端 API Key，默认自动生成
#   ADMIN_API_KEY 管理端 API Key，默认自动生成
#   REGION        Kiro 区域，默认 us-east-1
#   AUTO_START    安装后是否立即启动，默认 yes
#

set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-$HOME/kiro-rs}"
IMAGE_OWNER="${IMAGE_OWNER:-easayliu}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_REG="${IMAGE_REG:-ghcr.io}"
PORT="${PORT:-8990}"
REGION="${REGION:-us-east-1}"
AUTO_START="${AUTO_START:-yes}"

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BLUE=$'\033[34m'; BOLD=$'\033[1m'; RESET=$'\033[0m'

info()  { printf '%s[info]%s %s\n'  "$BLUE"   "$RESET" "$*"; }
warn()  { printf '%s[warn]%s %s\n'  "$YELLOW" "$RESET" "$*"; }
error() { printf '%s[error]%s %s\n' "$RED"    "$RESET" "$*" >&2; }
ok()    { printf '%s[ok]%s %s\n'    "$GREEN"  "$RESET" "$*"; }

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    error "缺少依赖：$1，请先安装"
    exit 1
  fi
}

detect_compose() {
  if docker compose version >/dev/null 2>&1; then
    echo "docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    echo "docker-compose"
  else
    error "未检测到 docker compose / docker-compose"
    exit 1
  fi
}

gen_key() {
  local prefix="$1"
  if command -v openssl >/dev/null 2>&1; then
    printf '%s-%s' "$prefix" "$(openssl rand -hex 16)"
  else
    printf '%s-%s%s' "$prefix" "$(date +%s)" "$RANDOM"
  fi
}

# 从 JSON 文件中按字符串键提取顶层字符串值（不依赖 jq，仅适用于简单 JSON）。
extract_json_string() {
  local file="$1" key="$2"
  [[ -f "$file" ]] || return 1
  awk -v k="\"$key\"" '
    {
      i = index($0, k)
      if (i == 0) next
      rest = substr($0, i + length(k))
      sub(/^[[:space:]]*:[[:space:]]*"/, "", rest)
      end = index(rest, "\"")
      if (end == 0) next
      print substr(rest, 1, end - 1)
      exit
    }
  ' "$file"
}

main() {
  require_cmd docker
  local COMPOSE
  COMPOSE="$(detect_compose)"
  ok "docker 就绪；compose 命令：$COMPOSE"

  mkdir -p "$INSTALL_DIR/config"
  info "安装目录：$INSTALL_DIR"

  # ---------- config.json ----------
  # 复用策略：若 config.json 已存在，则从中读取既有 apiKey / adminApiKey，
  # 避免重装时显示「新生成」的 key 与文件内容不一致，导致用户以为凭据被改写。
  local CONFIG_PATH="$INSTALL_DIR/config/config.json"
  local API_KEY_VAL ADMIN_API_KEY_VAL
  if [[ -f "$CONFIG_PATH" ]]; then
    API_KEY_VAL="${API_KEY:-$(extract_json_string "$CONFIG_PATH" apiKey)}"
    ADMIN_API_KEY_VAL="${ADMIN_API_KEY:-$(extract_json_string "$CONFIG_PATH" adminApiKey)}"
    [[ -n "$API_KEY_VAL" ]]       || API_KEY_VAL="$(gen_key sk-kiro)"
    [[ -n "$ADMIN_API_KEY_VAL" ]] || ADMIN_API_KEY_VAL="$(gen_key sk-admin)"
    warn "已存在 ${CONFIG_PATH}，跳过覆盖（沿用既有 apiKey / adminApiKey）"
  else
    API_KEY_VAL="${API_KEY:-$(gen_key sk-kiro)}"
    ADMIN_API_KEY_VAL="${ADMIN_API_KEY:-$(gen_key sk-admin)}"
    cat > "$CONFIG_PATH" <<EOF
{
  "host": "0.0.0.0",
  "port": $PORT,
  "apiKey": "$API_KEY_VAL",
  "tlsBackend": "rustls",
  "region": "$REGION",
  "adminApiKey": "$ADMIN_API_KEY_VAL"
}
EOF
    ok "已写入 $CONFIG_PATH"
  fi

  # ---------- credentials.json ----------
  # 使用多凭据数组格式，确保 Admin API 新增/修改凭据可被回写到文件，
  # 避免容器重启后新增凭据丢失（单凭据对象格式不会被 persist_credentials 回写）。
  local CREDS_PATH="$INSTALL_DIR/config/credentials.json"
  if [[ -f "$CREDS_PATH" ]]; then
    warn "已存在 $CREDS_PATH，跳过覆盖"
  else
    cat > "$CREDS_PATH" <<'EOF'
[
  {
    "kiroApiKey": "ksk_请填入你的_kiro_api_key",
    "authMethod": "api_key"
  }
]
EOF
    warn "已写入 $CREDS_PATH 占位文件，请编辑并填入真实凭据（支持 api_key / social / idc）"
  fi

  # ---------- docker-compose.yml ----------
  local COMPOSE_PATH="$INSTALL_DIR/docker-compose.yml"
  cat > "$COMPOSE_PATH" <<EOF
services:
  kiro-rs:
    image: ${IMAGE_REG}/${IMAGE_OWNER}/kiro-rs:${IMAGE_TAG}
    container_name: kiro-rs
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      - "${PORT}:${PORT}"
    volumes:
      - ./config/:/app/config/
    restart: unless-stopped
EOF
  ok "已写入 $COMPOSE_PATH"

  # ---------- Pull & Start ----------
  if [[ "$AUTO_START" != "yes" ]]; then
    info "AUTO_START=no，跳过启动"
    print_summary "$API_KEY_VAL" "$ADMIN_API_KEY_VAL"
    return
  fi

  (
    cd "$INSTALL_DIR"
    info "拉取镜像 ${IMAGE_REG}/${IMAGE_OWNER}/kiro-rs:${IMAGE_TAG} ..."
    $COMPOSE pull
    info "启动容器 ..."
    $COMPOSE up -d
  )

  ok "启动完成"
  print_summary "$API_KEY_VAL" "$ADMIN_API_KEY_VAL"
}

print_summary() {
  local api_key="$1" admin_key="$2"
  cat <<EOF

${BOLD}${GREEN}✓ kiro-rs 安装完成${RESET}

  目录:        ${INSTALL_DIR}
  Anthropic:   http://127.0.0.1:${PORT}/v1
  Admin API:   http://127.0.0.1:${PORT}/admin
  API Key:     ${api_key}
  Admin Key:   ${admin_key}

常用命令（在 ${INSTALL_DIR} 目录下执行）:
  查看日志     docker compose logs -f
  停止         docker compose down
  升级         docker compose pull && docker compose up -d
  编辑凭据     ${INSTALL_DIR}/config/credentials.json 后 docker compose restart

客户端使用（以 Claude Code 为例）:
  export ANTHROPIC_BASE_URL=http://127.0.0.1:${PORT}
  export ANTHROPIC_AUTH_TOKEN=${api_key}

EOF
}

main "$@"
