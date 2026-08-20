---
status: progress
type: evidence
authority: verification
---

# Network Carrier Latency Experiment

This experiment determines whether latency-sensitive remote messages should use
QUIC streams, QUIC DATAGRAM, or a separate UDP socket. It also measures whether
bulk traffic sharing the QUIC connection changes that result.

The executable is
`crates/brood-transport/examples/network-latency-experiment.rs`. It runs an
ephemeral authenticated QUIC echo endpoint and a raw UDP echo endpoint. The
client measures all three carriers concurrently so a host or route stall is
observed by every carrier in the same time window rather than being
misattributed to whichever sequential phase happened to be active.

## Method

The remote endpoint ran on `luna` and the client used the same Tailscale address
as the SSH deployment workflow. Each controlled trial used:

- 500 request/echo samples per carrier;
- 128-byte messages at 10 ms intervals;
- the production QUIC acknowledgement configuration;
- one QUIC stream, QUIC DATAGRAM on the same connection, and a raw UDP socket
  served by a dedicated blocking thread;
- an idle phase followed by a concurrent 0.1 MiB/s QUIC bulk stream;
- three repeated trials.

The following values are ranges across those three trials, in milliseconds:

| Load | Carrier | p95 | p99 | standard deviation |
| --- | --- | ---: | ---: | ---: |
| idle | QUIC stream | 13.60-15.30 | 26.14-30.60 | 3.76-4.75 |
| idle | QUIC DATAGRAM | 13.32-15.66 | 27.93-35.41 | 3.81-4.95 |
| idle | raw UDP thread | 12.51-14.01 | 18.64-26.44 | 3.25-3.99 |
| bulk | QUIC stream | 16.15-17.45 | 20.92-35.33 | 6.55-24.09 |
| bulk | QUIC DATAGRAM | 16.56-17.43 | 21.12-33.13 | 6.34-24.05 |
| bulk | raw UDP thread | 14.59-16.06 | 21.45-30.24 | 5.98-23.92 |

QUIC DATAGRAM did not improve p95, p99, or deviation relative to the QUIC
stream. The dedicated UDP path usually saved approximately 1-2 ms, but it did
not remove the large stalls.

The large bulk-phase outliers were correlated across all carriers:

| Trial | QUIC stream maximum | QUIC DATAGRAM maximum | raw UDP maximum |
| --- | ---: | ---: | ---: |
| 1 | 120.23 ms | 120.16 ms | 116.42 ms |
| 2 | 304.24 ms | 304.14 ms | 303.82 ms |
| 3 | 537.55 ms | 537.48 ms | 536.13 ms |

A fourth trial ran ICMP continuously in the same measurement window. ICMP
reported a 52.22 ms maximum and 4.05 ms deviation while the three application
carriers reported approximately 48-51 ms maxima and 3.85-4.16 ms deviation.
That correlation places the dominant variance below the application carrier:
the shared host network stack, Tailscale path, or physical route.

An overload probe also showed why bulk traffic still needs pacing. At 1 MiB/s,
p95 rose to 141 ms for the QUIC stream, 188 ms for QUIC DATAGRAM, and 115 ms for
the separate UDP socket. Separate sockets cannot overcome a saturated shared
route.

## Decision

Do not replace normal messages with a custom reliable UDP protocol based on
these results. QUIC DATAGRAM shares the same tail as QUIC streams, and raw UDP
does not escape the dominant route-level stalls.

Retain reliable QUIC streams for lossless messages. A datagram path remains
appropriate for genuinely supersedable latest-state messages, but it is a
semantic optimization rather than the primary latency fix.

Put bulk data behind explicit pacing and backpressure. A separate connection
can isolate flow-control and application scheduling, but it cannot guarantee
interactive latency when both connections saturate the same physical route.

The next diagnostic should identify the shared layer by comparing the direct
LAN address against Tailscale and recording endpoint CPU scheduling plus kernel
socket timestamps during the same concurrent probe.
