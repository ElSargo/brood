#!/usr/bin/env python3

import argparse
import subprocess
import time


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--socket", required=True)
    parser.add_argument("--count", type=int, required=True)
    parser.add_argument("--interval-us", type=int, required=True)
    parser.add_argument("key")
    args = parser.parse_args()

    control = subprocess.Popen(
        ["tmux", "-S", args.socket, "-C", "attach"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert control.stdin is not None
    started = time.monotonic_ns()
    interval_ns = args.interval_us * 1_000
    for index in range(args.count):
        control.stdin.write(f"send-keys {args.key}\n")
        control.stdin.flush()
        deadline = started + (index + 1) * interval_ns
        remaining = deadline - time.monotonic_ns()
        if remaining > 0:
            time.sleep(remaining / 1_000_000_000)
    control.stdin.close()
    control.terminate()
    control.wait(timeout=5)


if __name__ == "__main__":
    main()
