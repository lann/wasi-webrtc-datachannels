#!/usr/bin/env bash
#
# Provision (or tear down) the conformance ICE-lab network namespaces: the
# offerer, answerer, signaling, and router namespaces and the veth links between
# them (see lib.sh for the topology). Idempotent: `up` first tears down any stale
# lab, and both `up` and `down` are safe to run repeatedly.
#
# Usage:
#   netns.sh up      # create namespaces, veths, addresses, routes, forwarding
#   netns.sh down     # remove everything this script created
#
# Requires root (or passwordless sudo) for `ip netns`. Usable standalone.

set -euo pipefail

CW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$CW_DIR/lib.sh"

# Create one router<->endpoint link: a veth pair with the router end in cw-rtr
# and the endpoint end in `ns`, addressed and routed so the endpoint reaches the
# whole lab through the router.
cw_link() {
    local ns="$1" veth_ep="$2" veth_rtr="$3" ep_addr="$4" gw_addr="$5"

    cw_priv ip link add "$veth_ep" type veth peer name "$veth_rtr"
    cw_priv ip link set "$veth_ep" netns "$ns"
    cw_priv ip link set "$veth_rtr" netns "$CW_NS_RTR"

    # Endpoint side.
    cw_ns "$ns" ip addr add "$ep_addr/30" dev "$veth_ep"
    cw_ns "$ns" ip link set "$veth_ep" up
    cw_ns "$ns" ip link set lo up
    cw_ns "$ns" ip route add default via "$gw_addr"

    # Router side.
    cw_ns "$CW_NS_RTR" ip addr add "$gw_addr/30" dev "$veth_rtr"
    cw_ns "$CW_NS_RTR" ip link set "$veth_rtr" up
}

cw_up() {
    # Start from a clean slate so a half-provisioned lab never wedges a run.
    cw_down

    cw_log "creating namespaces"
    cw_priv ip netns add "$CW_NS_RTR"
    cw_priv ip netns add "$CW_NS_OFF"
    cw_priv ip netns add "$CW_NS_ANS"
    cw_priv ip netns add "$CW_NS_SIG"

    cw_ns "$CW_NS_RTR" ip link set lo up
    # The router forwards between the three /30 links.
    cw_ns "$CW_NS_RTR" sysctl -q -w net.ipv4.ip_forward=1

    cw_log "wiring links"
    cw_link "$CW_NS_OFF" veth-off veth-roff "$CW_OFF_ADDR" "$CW_OFF_GW"
    cw_link "$CW_NS_ANS" veth-ans veth-rans "$CW_ANS_ADDR" "$CW_ANS_GW"
    cw_link "$CW_NS_SIG" veth-sig veth-rsig "$CW_SIG_ADDR" "$CW_SIG_GW"

    cw_log "lab ready"
}

cw_down() {
    # Deleting a namespace removes the veth ends inside it; the peer ends go with
    # them. Ignore errors so teardown is idempotent.
    for ns in "$CW_NS_OFF" "$CW_NS_ANS" "$CW_NS_SIG" "$CW_NS_RTR"; do
        cw_priv ip netns del "$ns" 2>/dev/null || true
    done
}

case "${1:-}" in
    up) cw_up ;;
    down) cw_down ;;
    *)
        echo "usage: $0 {up|down}" >&2
        exit 2
        ;;
esac
