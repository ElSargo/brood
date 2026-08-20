---
status: progress
type: evidence
authority: verification
last-reviewed: 2026-07-28
---

# Terminal Latency Spike Profile

## Conclusion

The normal interactive path is already close to the route floor. With 30 ms
between keys, 237 end-to-end samples had a 7.75 ms median, 10.36 ms p95,
14.56 ms p99, and 2.22 ms population standard deviation. The remote round trip
accounted for 97.1% of observed variance. Local input queueing, actor dispatch,
terminal rendering, and presentation together added about 0.73 ms on average.

The previously reported local render tail was primarily a benchmark overload
artifact. The harness used `wtype -s 30`, which sleeps once before interpreting
the following arguments. It did not delay between keys. Inputs actually arrived
about every 4.18 ms, or approximately 239 keys/s. At that rate, several terminal
revisions queued behind one scene-ready/presentation turn. Correcting the
workload to `wtype -d 30` reduced plugin-render p95 from 12.43 ms to 0.27 ms and
its standard deviation from 4.53 ms to 0.29 ms.

A custom reliable UDP protocol is therefore not the first optimization to
pursue. The application queues and remote relay add tens or hundreds of
microseconds, while the remaining normal-input tail tracks the shared network
route. A separate datagram path cannot remove a Tailscale, host-network, or
physical-route stall.

## Measurement boundary

Every terminal input now carries a monotonically increasing sequence through
the complete path:

`key -> presentation actor -> transport queue -> QUIC interactive lane ->
remote relay -> backend IPC -> PTY write -> PTY output -> terminal snapshot ->
client actor dispatch -> tagged graphics scene -> presented frame`

The session snapshot acknowledges a sequence only after all bytes for that
input were written to the PTY and corresponding PTY output was consumed. Relay
timings use the same sequence rather than FIFO inference. The client associates
the acknowledged sequence with the accepted graphics scene, so a later scene
which supersedes an earlier one can legitimately cover both inputs.

The recorded stages are:

| Stage | Boundary |
| --- | --- |
| input to transport queue | key accepted to network command admitted |
| transport send queue | admitted to QUIC lane write |
| remote round trip | QUIC write to acknowledged snapshot received locally |
| client dispatch | snapshot received to presentation actor mailbox dispatch |
| plugin render | mailbox dispatch to a ready graphics scene covering the input |
| presentation | graphics-ready notification to completed client presentation |

## Method

All trials used the checked-in Nix environment, release builds, the real
offscreen nested presentation client, a real shell PTY, and the real SSH
deployment workflow to `luna`. Each run injected 80 printable echo inputs and
captured:

- raw per-input stage timings;
- sequence-keyed remote relay timings;
- QUIC RTT, loss, congestion, congestion-window, and MTU diagnostics;
- concurrent 100 Hz ICMP samples for four seconds;
- aggregate `/proc` scheduler and context-switch deltas across all client
  threads;
- `perf stat` counters.

Three corrected burst runs used the original effective rate, approximately
239 keys/s. Three corrected interactive runs used a 30 ms inter-key delay.
The interactive runs yielded 79 completed trace records each, for 237 pooled
samples. Percentiles below pool the raw samples; standard deviation is the
population standard deviation. Per-run p99 with only 79 or 80 samples is
effectively a maximum, so the pooled values are more useful.

The proof artifacts are in `/tmp/brood-full-trace-7` through
`/tmp/brood-full-trace-12`. A separate sampling run is in
`/tmp/brood-full-trace-6`; it is excluded from latency aggregates because stack
sampling and verbose graphics tracing can perturb timings.

A final non-aggregate smoke in `/tmp/brood-full-trace-14` exercised the
multi-chunk revision boundary after the report dataset was collected. It
reproduced a 7.46 ms median, 8.69 ms p95, 1.48 ms standard deviation, 0.070 ms
mean plugin render, and a remote-round-trip maximum which again supplied the
end-to-end maximum.

## Interactive results

### End to end by run

All values are milliseconds.

| Run | samples | mean | p50 | p95 | p99 | maximum | stddev |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10 | 79 | 8.083 | 7.632 | 10.307 | 34.926 | 34.926 | 3.214 |
| 11 | 79 | 8.173 | 7.910 | 10.353 | 19.744 | 19.744 | 1.677 |
| 12 | 79 | 7.881 | 7.683 | 10.711 | 12.093 | 12.093 | 1.269 |
| pooled | 237 | 8.046 | 7.752 | 10.360 | 14.555 | 34.926 | 2.221 |

### Pooled stage attribution

| Stage | mean | p50 | p95 | p99 | maximum | stddev | share of total variance |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| input to transport queue | 0.046 | 0.035 | 0.093 | 0.249 | 0.252 | 0.040 | approximately 0% |
| transport send queue | 0.014 | 0.013 | 0.025 | 0.039 | 0.059 | 0.006 | approximately 0% |
| remote round trip | 7.313 | 7.027 | 9.797 | 11.701 | 34.152 | 2.182 | 97.10% |
| client dispatch | 0.002 | 0.002 | 0.004 | 0.004 | 0.011 | 0.001 | approximately 0% |
| plugin render | 0.125 | 0.076 | 0.266 | 1.403 | 3.853 | 0.290 | 2.52% |
| presentation | 0.543 | 0.528 | 0.802 | 0.954 | 1.466 | 0.124 | 0.38% |

The correlation between remote round trip and total latency was 0.988. The
largest application round trip in run 10 was 34.15 ms; ICMP in the same
measurement window reached 34.32 ms. Run 11 reached 19.32 ms in the application
and 23.71 ms in ICMP. Run 12 reached 10.85 ms in the application and 17.03 ms
in ICMP. This is strong run-level evidence that the remaining tail is below the
application carrier.

QUIC reported a final RTT of 6.16-6.76 ms, zero congestion events, and zero
lost QUIC packets in all three runs. The remote relay itself was small:

| Remote relay stage | mean | p50 range | p95 range | maximum | pooled stddev |
| --- | ---: | ---: | ---: | ---: | ---: |
| network receive to backend IPC | 0.029 | 0.015-0.016 | 0.049-0.162 | 0.442 | 0.050 |
| backend IPC to snapshot | 0.561 | 0.537-0.545 | 0.667-0.766 | 3.121 | 0.207 |
| complete backend turnaround | 0.591 | 0.559-0.565 | 0.696-0.790 | 3.135 | 0.199 |
| snapshot to QUIC send | 0.058 | 0.039-0.040 | 0.057-0.067 | 3.678 | 0.229 |

## Burst results and source of the old tail

With the original effective 239 keys/s workload, 240 pooled samples measured:

| Metric | mean | p50 | p95 | p99 | maximum | stddev |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| end to end | 15.535 | 15.543 | 23.141 | 24.236 | 25.055 | 4.794 |
| remote round trip | 7.100 | 7.119 | 8.626 | 9.442 | 12.219 | 0.988 |
| plugin render | 5.557 | 4.534 | 12.426 | 14.721 | 15.974 | 4.533 |
| presentation | 2.825 | 2.602 | 5.700 | 9.994 | 12.227 | 1.919 |

Plugin render accounted for 85.2% of burst variance and presentation for 9.6%.
Raw traces show a repeating high-to-low latency ramp as four or five inputs
arrive within one approximately 16 ms scene/presentation interval. The terminal
still accepts one draw packet per echoed input; later ready scenes supersede
earlier pending scenes and cover their input sequences. This is queueing under
an input rate above the presentation cadence, not glyph shaping or raster work.

Verbose tracing in the sampling run measured text layout and glyph raster work
below 0.12 ms for the echoed terminal row. It is not the source of the
multi-millisecond burst tail.

## CPU and scheduler profile

Across the corrected burst runs, the 116 client threads accumulated 4.31-4.93
CPU-seconds during each four-second profile window. Aggregate runnable wait was
only 1.3-6.7 ms, despite 13,804-15,692 voluntary and 80-158 involuntary context
switches. There is no evidence of CPU scheduler starvation causing the latency
tail.

The sampling run collected approximately 17,000 samples and 13.23 billion
cycles. It is qualitative because verbose tracing was enabled. The largest
self-time groups were:

- allocation, deallocation, vector construction, and reallocation: roughly
  30%;
- Winit/Wayland event pumping and flushing: roughly 13%;
- environment-variable lookup: roughly 7%;
- repeated scale-factor queries: 4.6%;
- layout-state publication: 1.2%.

These costs explain the high steady CPU use and are worthwhile cleanup, but
they do not correlate with the normal-input tail.

## Optimization order

1. **Keep the corrected workload and exact sequence markers.** Treat paced and
   burst results as different acceptance classes. A latency gate must use
   `wtype -d`, not `wtype -s`.
2. **Diagnose the route before replacing QUIC.** Repeat the concurrent probe
   over direct LAN and Tailscale addresses, with endpoint CPU scheduling and
   kernel socket timestamps. The current application overhead is already below
   one millisecond for paced input.
3. **Optimize burst supersession.** Keep at most the newest unrendered terminal
   revision, wake the presentation actor immediately for interactive snapshots,
   and avoid compiling scenes that are already superseded. This targets the
   4.53 ms burst render standard deviation without changing terminal semantics.
4. **Make presentation latest-state driven.** Coalesce redraw requests to the
   newest ready scene and avoid a blocking GPU-consumption wait where the
   platform can safely pipeline submissions. This is secondary for normal
   input but material under burst load.
5. **Remove hot-loop CPU churn.** Cache diagnostic environment flags at startup,
   publish layout state only when dirty, reuse temporary vectors/maps, and cache
   the window scale factor until a scale event.
6. **Retain semantic network lanes and bulk pacing.** Interactive actor traffic
   is already isolated from bulk and frame-resource streams. A datagram lane is
   useful only for genuinely supersedable state; it is not a remedy for the
   measured route spikes.

## Decision

Do not start a custom UDP reliability implementation for terminal messages.
The paced terminal adds approximately 0.73 ms beyond the remote round trip, and
the remaining variance follows the shared route. First fix route selection and
burst supersession. Reconsider the carrier only if a direct-LAN comparison
shows a reproducible QUIC-specific delta after those changes.
