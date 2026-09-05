#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Goal verificator for ubuntu-miracast-server-rs.
#
# GOAL: an Android phone's Smart View/Cast must DISCOVER this sink. Per the
# WFD/AOSP discovery model, the phone finds a sink via P2P device discovery
# (Probe Request/Response on the 2.4GHz social channels 1/6/11) and filters by
# the WFD Information Element — NOT by reading the GO beacon. So "discoverable"
# means: a SECOND P2P device, scanning, sees our sink as a P2P peer carrying a
# WFD IE, on a social channel. This script checks exactly that, WITHOUT needing
# the phone.
#
# It uses the SYSTEM wpa_supplicant on the built-in adapter (the "observer") to
# p2p_find and inspect the peer table for the sink started on the USB adapter.
#
# Usage:  sudo ./scripts/verify_discoverable.sh [observer_iface] [device_name]
#   observer_iface : the radio to scan FROM (default: auto-detect a wifi iface
#                    that is NOT the one the sink uses). e.g. wlo1
#   device_name    : the sink's advertised name (default: "Ubuntu Miracast Server")
#
# Exit 0 = sink is discoverable (goal met for the discovery phase).
# Exit 1 = sink NOT discovered. Exit 2 = setup/precondition failure.
# ---------------------------------------------------------------------------
set -uo pipefail

OBSERVER_IFACE="${1:-}"
DEVICE_NAME="${2:-Ubuntu Miracast Server}"
BIN="./target/release/ubuntu-miracast-server"
FIND_SECONDS=25
LOG="${KIROCREW_SCRATCH:-/tmp}/miracast-verify-$$.log"

say()  { printf '\033[1;36m[verify]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[verify FAIL]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[verify OK]\033[0m %s\n' "$*"; }

if [ "$(id -u)" -ne 0 ]; then
  fail "run with sudo (P2P scan + control socket need privilege here)"
  exit 2
fi

# --- pick an observer interface that is NOT going to be the sink's adapter ----
if [ -z "$OBSERVER_IFACE" ]; then
  # Prefer a connected/managed wifi iface as the observer (the sink takes the
  # idle one). Fall back to the first wifi iface.
  OBSERVER_IFACE="$(nmcli -t -f DEVICE,TYPE,STATE dev status 2>/dev/null \
    | awk -F: '$2=="wifi" && $3=="connected"{print $1; exit}')"
  [ -z "$OBSERVER_IFACE" ] && OBSERVER_IFACE="$(ls /sys/class/net | grep -E '^wl' | head -1)"
fi
if [ -z "$OBSERVER_IFACE" ]; then
  fail "no wifi interface found to scan from"
  exit 2
fi
say "observer interface: $OBSERVER_IFACE"
say "looking for sink device_name: '$DEVICE_NAME'"

# --- start the sink in the background ----------------------------------------
if [ ! -x "$BIN" ]; then
  fail "binary not found: $BIN (build first: cargo build --release --features gui)"
  exit 2
fi
say "starting sink ($BIN) ..."
RUST_LOG=info "$BIN" >"$LOG" 2>&1 &
SINK_PID=$!
cleanup() {
  say "stopping sink (pid $SINK_PID) ..."
  kill "$SINK_PID" 2>/dev/null
  wait "$SINK_PID" 2>/dev/null
}
trap cleanup EXIT

# Wait for the GO to come up (look for the log line).
for _ in $(seq 1 20); do
  grep -q "P2P GO active on" "$LOG" && break
  if ! kill -0 "$SINK_PID" 2>/dev/null; then
    fail "sink exited during startup — log tail:"; tail -15 "$LOG"; exit 2
  fi
  sleep 1
done
if ! grep -q "P2P GO active on" "$LOG"; then
  fail "sink did not report an active GO within 20s — log tail:"; tail -20 "$LOG"; exit 2
fi
GROUP_IFACE="$(grep -oE 'P2P GO active on [^ ]+' "$LOG" | tail -1 | awk '{print $NF}')"
ok "sink GO is up (group iface: ${GROUP_IFACE:-unknown})"

# --- confirm the GO is on a 2.4GHz social channel ----------------------------
if [ -n "${GROUP_IFACE:-}" ]; then
  CH="$(iw dev "$GROUP_IFACE" info 2>/dev/null | awk '/channel/{print $2}')"
  FREQ="$(iw dev "$GROUP_IFACE" info 2>/dev/null | grep -oE '\(([0-9]+) MHz\)' | grep -oE '[0-9]+' | head -1)"
  say "GO channel=$CH freq=${FREQ}MHz"
  case "$CH" in
    1|6|11) ok "GO is on a 2.4GHz social channel ($CH) — phones scan here" ;;
    *)      fail "GO is on channel $CH (${FREQ}MHz) — NOT a 2.4GHz social channel; a phone likely won't discover it" ;;
  esac
fi

# --- scan from the observer for the sink as a P2P peer with a WFD IE ----------
say "scanning from $OBSERVER_IFACE for the sink (p2p_find ${FIND_SECONDS}s) ..."
wpa_cli -i "$OBSERVER_IFACE" set wifi_display 1 >/dev/null 2>&1
wpa_cli -i "$OBSERVER_IFACE" p2p_find type=progressive >/dev/null 2>&1
DISCOVERED=""
for _ in $(seq 1 "$FIND_SECONDS"); do
  PEERS="$(wpa_cli -i "$OBSERVER_IFACE" p2p_peers 2>/dev/null)"
  for mac in $PEERS; do
    INFO="$(wpa_cli -i "$OBSERVER_IFACE" p2p_peer "$mac" 2>/dev/null)"
    if echo "$INFO" | grep -qiF "device_name=$DEVICE_NAME"; then
      DISCOVERED="$mac"
      WFD="$(echo "$INFO" | grep -iE 'wfd_dev_info|wfd_subelems' || true)"
      break 2
    fi
  done
  sleep 1
done
wpa_cli -i "$OBSERVER_IFACE" p2p_stop_find >/dev/null 2>&1

echo "---------------------------------------------------------------"
if [ -n "$DISCOVERED" ]; then
  ok "DISCOVERED sink '$DEVICE_NAME' as P2P peer $DISCOVERED"
  if [ -n "${WFD:-}" ]; then
    ok "sink advertises a WFD IE: $WFD"
    ok "GOAL MET (discovery): an Android phone would list this sink in Cast."
    exit 0
  else
    fail "sink found as a P2P peer but NO WFD IE — Android's isWifiDisplay() filter would REJECT it."
    exit 1
  fi
else
  fail "sink '$DEVICE_NAME' was NOT discovered as a P2P peer in ${FIND_SECONDS}s."
  say  "sink log tail:"; tail -25 "$LOG"
  exit 1
fi
