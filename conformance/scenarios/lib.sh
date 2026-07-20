#!/usr/bin/env bash
#
# Shared configuration and helpers for the conformance ICE-lab scenario scripts.
#
# The lab is a small routed network of Linux network namespaces provisioned
# entirely with `ip`, `nft`, and `turnserver` (no containers). It gives the
# conformance adapters a realistic, reproducible topology in which two peers sit
# on separate subnets behind a router, so an ICE handshake exercises a real
# (non-loopback) network path — and so the router can selectively block the
# direct path to force server-reflexive (STUN) or relay (TURN) candidates.
#
# Topology (all addresses in 10.79.0.0/16, one /30 per link):
#
#        cw-off (offerer)            cw-ans (answerer)
#        10.79.1.2/30                10.79.2.2/30
#             | veth                      | veth
#        10.79.1.1                   10.79.2.1
#        +----------------- cw-rtr (router, ip_forward=1) ----------------+
#                                    10.79.3.1
#                                        | veth
#                                    10.79.3.2/30
#                                 cw-sig (signaling + coturn)
#
# The signaling server (conformance-signalingd) and the TURN/STUN server
# (coturn) both run in cw-sig, reachable from either peer through the router.
#
# This file is meant to be sourced; it only defines variables and helpers and
# does not act on its own. See scenario.sh, netns.sh, coturn.sh, and nftables.sh.

# The scenario config is intentionally centralized here so every script and the
# Rust orchestrator agree on names and addresses (the orchestrator reads the
# `cw scenario env` output, which is derived from these values).

# Namespace names.
CW_NS_OFF="${CW_NS_OFF:-cw-off}"
CW_NS_ANS="${CW_NS_ANS:-cw-ans}"
CW_NS_SIG="${CW_NS_SIG:-cw-sig}"
CW_NS_RTR="${CW_NS_RTR:-cw-rtr}"

# Per-link addresses (router side .1, endpoint side .2).
CW_OFF_ADDR="${CW_OFF_ADDR:-10.79.1.2}"
CW_OFF_GW="${CW_OFF_GW:-10.79.1.1}"
CW_ANS_ADDR="${CW_ANS_ADDR:-10.79.2.2}"
CW_ANS_GW="${CW_ANS_GW:-10.79.2.1}"
CW_SIG_ADDR="${CW_SIG_ADDR:-10.79.3.2}"
CW_SIG_GW="${CW_SIG_GW:-10.79.3.1}"

# Subnets (used by nftables direct-path blocking).
CW_OFF_SUBNET="${CW_OFF_SUBNET:-10.79.1.0/30}"
CW_ANS_SUBNET="${CW_ANS_SUBNET:-10.79.2.0/30}"
CW_SIG_SUBNET="${CW_SIG_SUBNET:-10.79.3.0/30}"

# Service ports in cw-sig.
CW_SIGNALING_PORT="${CW_SIGNALING_PORT:-8080}"
CW_TURN_PORT="${CW_TURN_PORT:-3478}"
CW_TURN_MIN_PORT="${CW_TURN_MIN_PORT:-49160}"
CW_TURN_MAX_PORT="${CW_TURN_MAX_PORT:-49400}"

# TURN long-term credentials and realm (shared by coturn and the peers).
CW_TURN_USER="${CW_TURN_USER:-conf}"
CW_TURN_PASS="${CW_TURN_PASS:-conf}"
CW_TURN_REALM="${CW_TURN_REALM:-conformance}"

# Where coturn's generated config, pidfile, and logs live.
CW_RUN_DIR="${CW_RUN_DIR:-/tmp/conformance-ice}"

# Whether `ip`/`nft`/`turnserver` need sudo. When already root, run directly.
if [ "$(id -u)" -eq 0 ]; then
    CW_SUDO=""
else
    CW_SUDO="sudo"
fi

# Print a message to stderr (keeps stdout clean for machine-readable output).
cw_log() {
    printf 'cw: %s\n' "$*" >&2
}

# Run a privileged command (via sudo when not root).
cw_priv() {
    # shellcheck disable=SC2086
    $CW_SUDO "$@"
}

# Run a command inside a namespace.
cw_ns() {
    local ns="$1"
    shift
    cw_priv ip netns exec "$ns" "$@"
}

# True when a namespace exists.
cw_ns_exists() {
    cw_priv ip netns list | grep -qx -- "$1" 2>/dev/null || \
        cw_priv ip netns list | grep -q -- "^$1\b"
}
