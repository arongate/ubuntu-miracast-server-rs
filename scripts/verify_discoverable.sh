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
# NOTE: `iw dev <p2p-go> info` does NOT print an operating channel line, so read
# the frequency from the phy's in-use survey instead.
if [ -n "${GROUP_IFACE:-}" ]; then
  GO_WIPHY="$(iw dev "$GROUP_IFACE" info 2>/dev/null | awk '/wiphy/{print $2}')"
  # Primary: the sink logs its GO operating frequency.
  FREQ="$(grep -oE 'P2P GO operating frequency: [0-9]+' "$LOG" | tail -1 | grep -oE '[0-9]+')"
  # Fallback 1: the phy's in-use survey.
  [ -z "$FREQ" ] && FREQ="$(iw dev "$GROUP_IFACE" survey dump 2>/dev/null \
           | awk '/\[in use\]/{print $2}' | head -1)"
  if [ -z "$FREQ" ]; then
    # Fallback 2: scan the phy for our own BSS by the GO's MAC.
    GO_MAC="$(iw dev "$GROUP_IFACE" info 2>/dev/null | awk '/addr/{print $2}')"
    FREQ="$(iw dev "$GROUP_IFACE" scan dump 2>/dev/null | awk -v m="$GO_MAC" '
            /^BSS/{bss=$2} /freq:/{f=$2} bss ~ m {print f; exit}')"
  fi
  say "GO wiphy=phy${GO_WIPHY} freq=${FREQ:-unknown}MHz"
  case "$FREQ" in
    2412|2437|2462) ok "GO is on a 2.4GHz social channel (${FREQ}MHz) — phones scan here" ;;
    2*)             say "WARN: GO is on 2.4GHz ${FREQ}MHz but not a social channel (2412/2437/2462)" ;;
    5*)             fail "GO is on 5GHz (${FREQ}MHz) — a phone will NOT discover it via P2P; force freq=2412" ;;
    *)              say "NOTE: could not read GO frequency (P2P-GO exposes none via 'iw info'); relying on the scan below" ;;
  esac
fi

# --- scan from the observer for the sink as a P2P peer with a WFD IE ----------
say "scanning from $OBSERVER_IFACE for the sink (p2p_find ${FIND_SECONDS}s) ..."
# Trust check: if the observer shares the GO's physical radio, "discovered" is
# NOT proof a phone (a separate radio) can find it — a radio always hears its
# own GO. Warn loudly so a same-radio pass isn't mistaken for the real thing.
OBS_WIPHY="$(iw dev "$OBSERVER_IFACE" info 2>/dev/null | awk '/wiphy/{print $2}')"
if [ -n "${GO_WIPHY:-}" ] && [ "${OBS_WIPHY:-x}" = "${GO_WIPHY:-y}" ]; then
  say "WARN: observer ($OBSERVER_IFACE) and the GO share phy${GO_WIPHY} — a same-radio"
  say "      scan hears its own GO regardless of channel, so a PASS here is WEAK evidence."
  say "      For a trustworthy result the GO must be on the idle adapter and 2.4GHz."
fi

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
