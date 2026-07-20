#!/usr/bin/env bash
#
# Launch (or stop) coturn as the STUN/TURN server for the ICE lab, inside the
# signaling namespace (cw-sig). Used by the `stun-srflx` and `turn-relay`
# scenarios; the `lan` scenario needs no server. Idempotent and usable standalone
# (requires the lab from netns.sh to be up).
#
# Usage:
#   coturn.sh up      # generate config and start turnserver in cw-sig
#   coturn.sh down    # stop turnserver
#
# Requires root (or passwordless sudo) and `turnserver` on PATH (installed by
# scripts/setup.sh).

set -euo pipefail

CW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$CW_DIR/lib.sh"

CW_TURN_CONF="$CW_RUN_DIR/turnserver.conf"
CW_TURN_PID="$CW_RUN_DIR/turnserver.pid"
CW_TURN_LOG="$CW_RUN_DIR/turnserver.log"

cw_turn_conf() {
    cw_priv mkdir -p "$CW_RUN_DIR"
    # A minimal long-term-credential TURN/STUN server bound to cw-sig's address.
    # No TLS is configured (the lab is a closed, ephemeral network).
    #
    # The relay port range must cover the *whole run's* allocations, not just
    # the concurrently-running tests: peers never deallocate explicitly (they
    # exit, or are killed when a stalled attempt times out), so every
    # allocation holds its relay port for the TURN default lifetime (10
    # minutes) — longer than the run. Sizing for the worst case (every
    # two-peer test retried to its attempt limit, one allocation per peer)
    # keeps retries from exhausting the range and cascading into failures.
    cw_priv tee "$CW_TURN_CONF" >/dev/null <<EOF
listening-ip=$CW_SIG_ADDR
listening-port=$CW_TURN_PORT
relay-ip=$CW_SIG_ADDR
min-port=$CW_TURN_MIN_PORT
max-port=$CW_TURN_MAX_PORT
fingerprint
lt-cred-mech
realm=$CW_TURN_REALM
user=$CW_TURN_USER:$CW_TURN_PASS
no-tls
no-dtls
no-cli
pidfile=$CW_TURN_PID
log-file=$CW_TURN_LOG
simple-log
EOF
}

cw_up() {
    command -v turnserver >/dev/null 2>&1 || {
        cw_log "turnserver not found on PATH (install coturn; see scripts/setup.sh)"
        exit 1
    }
    cw_down
    cw_turn_conf
    cw_log "starting coturn in $CW_NS_SIG on $CW_SIG_ADDR:$CW_TURN_PORT"
    # Run detached inside the signaling namespace. coturn daemonizes with -o.
    cw_ns "$CW_NS_SIG" turnserver -c "$CW_TURN_CONF" -o
    # Give it a moment to bind before the orchestrator points peers at it.
    sleep 1
}

cw_down() {
    if [ -f "$CW_TURN_PID" ]; then
        local pid
        pid="$(cw_priv cat "$CW_TURN_PID" 2>/dev/null || true)"
        if [ -n "$pid" ]; then
            cw_priv kill "$pid" 2>/dev/null || true
        fi
    fi
    # Belt and suspenders: kill any turnserver bound to our config.
    cw_priv pkill -f "turnserver -c $CW_TURN_CONF" 2>/dev/null || true
    cw_priv rm -f "$CW_TURN_PID" 2>/dev/null || true
}

case "${1:-}" in
    up) cw_up ;;
    down) cw_down ;;
    *)
        echo "usage: $0 {up|down}" >&2
        exit 2
        ;;
esac
