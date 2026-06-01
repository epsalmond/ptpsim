#!/usr/bin/env bash
#

#
# What this does:
#   1. Creates a service user `woodpecker` (no login shell).
#   2. Installs rustup + the four Apple targets under the woodpecker user.
#   3. Installs the gh CLI (Homebrew, system-wide).
#   4. Downloads the Woodpecker agent binary for darwin/amd64.
#   5. Installs a launchd plist (LaunchDaemon) that supervises the agent.
#   6. Boots the agent.
#
# Run as: sudo bash ci/setup-macos-agent.sh
#
# Required env vars (set before running):

#   WOODPECKER_AGENT_SECRET grab from Woodpecker UI → Admin → Agents → "New agent"
#
# Optional:
#   WOODPECKER_AGENT_VERSION  default: 3.10.0 (any v3.x should be fine)
#   AGENT_LABELS              default: platform=darwin,arch=x86_64,xcode=true
#
# Idempotent: re-running upgrades the agent, refreshes the plist, restarts.

set -euo pipefail

# ---------------------------------------------------------------------------
# Knobs
# ---------------------------------------------------------------------------


: "${WOODPECKER_AGENT_SECRET:?must set WOODPECKER_AGENT_SECRET (from Woodpecker admin UI)}"

WOODPECKER_AGENT_VERSION="${WOODPECKER_AGENT_VERSION:-3.10.0}"
AGENT_LABELS="${AGENT_LABELS:-platform=darwin,arch=x86_64,xcode=true}"

USER_NAME="woodpecker"
USER_HOME="/Users/${USER_NAME}"
AGENT_BIN_DIR="/usr/local/bin"
AGENT_BIN="${AGENT_BIN_DIR}/woodpecker-agent"
PLIST="/Library/LaunchDaemons/com.woodpecker.agent.plist"
LOG_DIR="/var/log/woodpecker"

if [[ "$(id -u)" != "0" ]]; then
    echo "FATAL: run with sudo" >&2
    exit 1
fi

if [[ "$(uname)" != "Darwin" ]]; then
    echo "FATAL: macOS only" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# 1. Service user
# ---------------------------------------------------------------------------

if ! id "${USER_NAME}" >/dev/null 2>&1; then
    echo "==> Creating service user ${USER_NAME}"
    # Pick an unused UID >= 500 (system convention); start at 700 to avoid clash.
    NEXT_UID=700
    while dscl . -read "/Users/_uid${NEXT_UID}" >/dev/null 2>&1 \
        || dscl . -search /Users UniqueID "${NEXT_UID}" 2>/dev/null | grep -q .; do
        NEXT_UID=$((NEXT_UID + 1))
    done
    dscl . -create "/Users/${USER_NAME}"
    dscl . -create "/Users/${USER_NAME}" UserShell /usr/bin/false
    dscl . -create "/Users/${USER_NAME}" RealName "Woodpecker CI"
    dscl . -create "/Users/${USER_NAME}" UniqueID "${NEXT_UID}"
    dscl . -create "/Users/${USER_NAME}" PrimaryGroupID 20  # staff
    dscl . -create "/Users/${USER_NAME}" NFSHomeDirectory "${USER_HOME}"
    mkdir -p "${USER_HOME}"
    chown "${USER_NAME}:staff" "${USER_HOME}"
else
    echo "==> User ${USER_NAME} already exists, skipping create"
fi

# ---------------------------------------------------------------------------
# 2. Rust + Apple targets (under woodpecker user)
# ---------------------------------------------------------------------------

if ! sudo -u "${USER_NAME}" -H bash -c 'command -v rustup' >/dev/null 2>&1; then
    echo "==> Installing rustup for ${USER_NAME}"
    sudo -u "${USER_NAME}" -H bash -c \
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
fi

echo "==> Ensuring Apple targets are installed for ${USER_NAME}"
sudo -u "${USER_NAME}" -H bash -lc '
    source $HOME/.cargo/env
    for t in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin; do
        rustup target add "$t"
    done
'

# ---------------------------------------------------------------------------
# 3. gh CLI (system-wide via Homebrew)
# ---------------------------------------------------------------------------

if ! command -v gh >/dev/null 2>&1; then
    echo "==> Installing gh CLI"
    if command -v brew >/dev/null 2>&1; then
        sudo -u "$(stat -f '%Su' "$(brew --prefix)")" brew install gh
    else
        echo "WARN: brew not present; install gh CLI manually before the release step runs" >&2
    fi
fi

# ---------------------------------------------------------------------------
# 4. Woodpecker agent binary
# ---------------------------------------------------------------------------


# would use the arm64 binary.
case "$(uname -m)" in
    x86_64) AGENT_ARCH="amd64" ;;
    arm64)  AGENT_ARCH="arm64" ;;
    *) echo "FATAL: unknown arch $(uname -m)" >&2; exit 1 ;;
esac

AGENT_URL="https://github.com/woodpecker-ci/woodpecker/releases/download/v${WOODPECKER_AGENT_VERSION}/woodpecker-agent_darwin_${AGENT_ARCH}.tar.gz"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

echo "==> Downloading woodpecker-agent v${WOODPECKER_AGENT_VERSION} darwin/${AGENT_ARCH}"
curl -fsSL "${AGENT_URL}" -o "${TMP}/agent.tar.gz"
tar -xzf "${TMP}/agent.tar.gz" -C "${TMP}"
install -m 0755 "${TMP}/woodpecker-agent" "${AGENT_BIN}"

# ---------------------------------------------------------------------------
# 5. launchd plist
# ---------------------------------------------------------------------------

mkdir -p "${LOG_DIR}"
chown "${USER_NAME}:staff" "${LOG_DIR}"

# Use the local backend (no Docker on macOS for Xcode builds). The agent
# runs commands directly on the host as the woodpecker user.
cat > "${PLIST}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.woodpecker.agent</string>
    <key>UserName</key>
    <string>${USER_NAME}</string>
    <key>WorkingDirectory</key>
    <string>${USER_HOME}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${AGENT_BIN}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>WOODPECKER_SERVER</key>
        <string>${WOODPECKER_SERVER}</string>
        <key>WOODPECKER_AGENT_SECRET</key>
        <string>${WOODPECKER_AGENT_SECRET}</string>
        <key>WOODPECKER_BACKEND</key>
        <string>local</string>
        <key>WOODPECKER_FILTER_LABELS</key>
        <string>${AGENT_LABELS}</string>
        <key>WOODPECKER_MAX_WORKFLOWS</key>
        <string>1</string>
        <key>PATH</key>
        <string>${USER_HOME}/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>StandardOutPath</key>
    <string>${LOG_DIR}/agent.log</string>
    <key>StandardErrorPath</key>
    <string>${LOG_DIR}/agent.err</string>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
EOF
chmod 0644 "${PLIST}"
chown root:wheel "${PLIST}"

# ---------------------------------------------------------------------------
# 6. Boot / restart
# ---------------------------------------------------------------------------

echo "==> (Re)loading launchd plist"
launchctl bootout system "${PLIST}" 2>/dev/null || true
launchctl bootstrap system "${PLIST}"
launchctl enable "system/com.woodpecker.agent"

echo
echo "Agent installed and started."
echo "  binary:    ${AGENT_BIN}"
echo "  plist:     ${PLIST}"
echo "  logs:      ${LOG_DIR}/agent.log + agent.err"
echo "  labels:    ${AGENT_LABELS}"
echo
echo "Verify with:"
echo "  sudo tail -f ${LOG_DIR}/agent.log"
echo "  launchctl print system/com.woodpecker.agent | head -20"
echo
echo "Confirm the agent shows up in Woodpecker UI (Admin → Agents) with the"
echo "labels above; pipeline steps with matching \`labels:\` will route here."
