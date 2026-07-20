#!/usr/bin/env bash
#
# Apply (or clear) the router's nftables policy that shapes which ICE candidate
# paths can carry data for a scenario. Runs in the router namespace (cw-rtr),
# whose forward chain sits between the offerer, answerer, and signaling links.
# Usable standalone (requires the lab from netns.sh to be up).
#
# Usage:
#   nftables.sh lan            # allow every path (host candidates connect directly)
#   nftables.sh stun-srflx     # port-restricted NAT + block direct path (srflx works)
#   nftables.sh turn-relay     # block the direct offerer<->answerer path (relay only)
#   nftables.sh nat-symmetric  # symmetric NAT + block direct path (srflx fails)
#   nftables.sh clear          # remove the policy
#
# All of `stun-srflx`, `turn-relay`, and `nat-symmetric` drop the direct path
# between the two peer subnets while leaving each peer's path to the
# signaling/coturn subnet open, so a successful connection must have traversed
# the server (server-reflexive or relayed candidates) rather than a direct
# host-candidate pair.
#
# The two NAT scenarios (Phase 6) additionally source-NAT each peer's forwarded
# traffic to its own "public" address (CW_OFF_PUB / CW_ANS_PUB), so the address
# the STUN server observes (the srflx candidate) differs from the peer's private
# host address. The mapping style decides whether srflx is usable:
#   stun-srflx    `snat ... persistent` gives a consistent, endpoint-independent
#                 mapping (a port-restricted cone NAT), so the two peers can
#                 hole-punch their srflx candidates and connect.
#   nat-symmetric `snat ... random` picks a fresh source port per destination (an
#                 endpoint-dependent, symmetric NAT), so the mapping the STUN
#                 server saw is useless to the peer and ICE must fall back to a
#                 TURN relay.
#
# Requires root (or passwordless sudo).

set -euo pipefail

CW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$CW_DIR/lib.sh"

CW_TABLE="cw_ice"

cw_clear() {
    cw_ns "$CW_NS_RTR" nft delete table inet "$CW_TABLE" 2>/dev/null || true
}

# The forward-chain rules that drop traffic directly between the offerer and
# answerer subnets (both directions), forcing a server-mediated path. Emitted
# into the table body by the builders below.
cw_block_direct_rules() {
    cat <<EOF
    chain forward {
        type filter hook forward priority 0; policy accept;
        ip saddr $CW_OFF_SUBNET ip daddr $CW_ANS_SUBNET drop
        ip saddr $CW_ANS_SUBNET ip daddr $CW_OFF_SUBNET drop
    }
EOF
}

# Install a table that only blocks the direct peer<->peer path (no NAT), used by
# turn-relay (the peers are relay-only, so no server-reflexive path is needed).
cw_block_direct() {
    cw_clear
    cw_ns "$CW_NS_RTR" nft -f - <<EOF
table inet $CW_TABLE {
$(cw_block_direct_rules)
}
EOF
}

# Install the direct-path block plus a source-NAT that rewrites each peer's
# forwarded traffic to its own public address. `mode` selects the mapping style:
#   persistent  endpoint-independent (cone) mapping — srflx is usable.
#   random      endpoint-dependent (symmetric) mapping — srflx is not usable.
cw_nat() {
    local mode="$1"
    cw_clear
    cw_ns "$CW_NS_RTR" nft -f - <<EOF
table inet $CW_TABLE {
$(cw_block_direct_rules)
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr $CW_OFF_SUBNET snat ip to $CW_OFF_PUB $mode
        ip saddr $CW_ANS_SUBNET snat ip to $CW_ANS_PUB $mode
    }
}
EOF
}

case "${1:-}" in
    lan) cw_clear ;;
    stun-srflx) cw_nat persistent ;;
    turn-relay) cw_block_direct ;;
    nat-symmetric) cw_nat random ;;
    clear) cw_clear ;;
    *)
        echo "usage: $0 {lan|stun-srflx|turn-relay|nat-symmetric|clear}" >&2
        exit 2
        ;;
esac
