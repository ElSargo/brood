# Manifold IPC

`manifold-ipc` owns the same-device presentation connection primitives.

The initial critical lane is a fixed-layout, bounded SPSC ring backed by a
size-sealed Linux memfd. Each endpoint maps the same region. A producer fills a
64-byte `CriticalRecord` in place and publishes it with a release store; the
consumer acquires the slot and borrows `&CriticalRecord` directly from shared
memory. Dropping the borrow releases the slot. The consumer performs no byte
parsing, allocation, or payload copy.

The active minimum-latency path spins. Event-loop integration will add a bounded
active spin followed by an armed notification so an idle connection does not
consume a core without adding notification syscalls to an already-active path.

Run the release benchmark through the checked-in environment:

```sh
nix develop --command cargo run --release -p manifold-ipc \
  --bin critical-lane-bench -- --iterations 200000 --warmup 20000
```

The benchmark compares:

- an in-process `std::sync::mpsc` ping/pong between threads;
- a fixed 64-byte Unix stream ping/pong between processes;
- two critical shared-memory lanes between processes.

Every ping carries its `CLOCK_MONOTONIC` send timestamp. The receiver records the
one-way observation time, returns it with the response, and the sender records
the round-trip time. Warmup samples and process startup are excluded. Run the
benchmark on the target device and inspect percentiles rather than treating one
machine's numbers as a portable guarantee.
