#!/usr/bin/env bash
#
# One-stop provisioning for a conformance ICE-lab scenario: bring the namespace
# topology (netns.sh), the STUN/TURN server (coturn.sh), and the router policy
# (nftables.sh) up or down together. This is the entry point the `just`
# recipes use, and it is usable standalone for interactive debugging.
#
# Usage:
#   scenario.sh up   <lan|stun-srflx|turn-relay>
#   scenario.sh down [scenario]     # scenario arg optional; tears the lab down
#   scenario.sh env  <lan|stun-srflx|turn-relay>
#
# `up` provisions the lab for the scenario; `down` removes everything; `env`
# prints the lab parameters (addresses, signaling URL, TURN credentials) as
# shell-sourceable KEY=VALUE lines, which the Rust orchestrator (conformance-ice)
# reads to place peers and point them at the signaling/coturn server.
#
# Scenarios:
#   lan          direct host-candidate connectivity over the router (no server).
#   stun-srflx   coturn as a STUN server; the direct peer<->peer path is blocked
#                so a server-reflexive path must be used. NOTE: a meaningful srflx
#                path also needs NAT on the router (see README / Phase 6); this
#                provisions the server + block, the NAT topology is layered on top.
#   turn-relay   coturn as a TURN server; the direct peer<->peer path is blocked,
#                and peers are relay-only, so data must be relayed by coturn.
#
# Requires root (or passwordless sudo).

set -euo pipefail

CW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$CW_DIR/lib.sh"

cw_valid_scenario() {
    case "$1" in
        lan | stun-srflx | turn-relay) return 0 ;;
        *) return 1 ;;
    esac
}

cw_up() {
    local scenario="$1"
    cw_valid_scenario "$scenario" || {
        echo "unknown scenario: $scenario" >&2
        exit 2
    }
    bash "$CW_DIR/netns.sh" up
    if [ "$scenario" != "lan" ]; then
        bash "$CW_DIR/coturn.sh" up
    fi
    bash "$CW_DIR/nftables.sh" "$scenario"
    cw_log "scenario '$scenario' ready"
}

cw_down() {
    bash "$CW_DIR/nftables.sh" clear || true
    bash "$CW_DIR/coturn.sh" down || true
    bash "$CW_DIR/netns.sh" down || true
    cw_log "lab torn down"
}

# Print the lab parameters the orchestrator needs, as KEY=VALUE lines.
cw_env() {
    local scenario="$1"
    cw_valid_scenario "$scenario" || {
        echo "unknown scenario: $scenario" >&2
        exit 2
    }
    cat <<EOF
CW_SCENARIO=$scenario
CW_NS_OFF=$CW_NS_OFF
CW_NS_ANS=$CW_NS_ANS
CW_NS_SIG=$CW_NS_SIG
CW_OFF_ADDR=$CW_OFF_ADDR
CW_ANS_ADDR=$CW_ANS_ADDR
CW_SIG_ADDR=$CW_SIG_ADDR
CW_SIGNALING_URL=http://$CW_SIG_ADDR:$CW_SIGNALING_PORT
CW_SIGNALING_PORT=$CW_SIGNALING_PORT
CW_TURN_URL=turn:$CW_SIG_ADDR:$CW_TURN_PORT
CW_STUN_URL=stun:$CW_SIG_ADDR:$CW_TURN_PORT
CW_TURN_USER=$CW_TURN_USER
CW_TURN_PASS=$CW_TURN_PASS
EOF
}

case "${1:-}" in
    up)
        cw_up "${2:?scenario required}"
        ;;
    down)
        cw_down
        ;;
    env)
        cw_env "${2:?scenario required}"
        ;;
    *)
        echo "usage: $0 {up <scenario>|down|env <scenario>}" >&2
        exit 2
        ;;
esac
