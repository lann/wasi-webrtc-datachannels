#!/usr/bin/env bash
#
# Apply (or clear) the router's nftables policy that shapes which ICE candidate
# paths can carry data for a scenario. Runs in the router namespace (cw-rtr),
# whose forward chain sits between the offerer, answerer, and signaling links.
# Usable standalone (requires the lab from netns.sh to be up).
#
# Usage:
#   nftables.sh lan          # allow every path (host candidates connect directly)
#   nftables.sh stun-srflx   # block the direct offerer<->answerer path
#   nftables.sh turn-relay   # block the direct offerer<->answerer path
#   nftables.sh clear         # remove the policy
#
# `stun-srflx` and `turn-relay` both drop the direct path between the two peer
# subnets while leaving each peer's path to the signaling/coturn subnet open, so
# a successful connection must have traversed the server (server-reflexive or
# relayed candidates) rather than a direct host-candidate pair.
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

# Install a forward chain that drops traffic directly between the offerer and
# answerer subnets (both directions) and accepts everything else.
cw_block_direct() {
    cw_clear
    cw_ns "$CW_NS_RTR" nft -f - <<EOF
table inet $CW_TABLE {
    chain forward {
        type filter hook forward priority 0; policy accept;
        ip saddr $CW_OFF_SUBNET ip daddr $CW_ANS_SUBNET drop
        ip saddr $CW_ANS_SUBNET ip daddr $CW_OFF_SUBNET drop
    }
}
EOF
}

case "${1:-}" in
    lan) cw_clear ;;
    stun-srflx | turn-relay) cw_block_direct ;;
    clear) cw_clear ;;
    *)
        echo "usage: $0 {lan|stun-srflx|turn-relay|clear}" >&2
        exit 2
        ;;
esac
