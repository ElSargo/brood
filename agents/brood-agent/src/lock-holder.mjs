// flock execs this process while holding the kernel lock. Losing the runner
// closes stdin, so even a SIGKILL releases the lock without PID-file recovery.
process.stdout.write("locked\n");
process.stdin.resume();
process.stdin.on("end", () => process.exit(0));
process.stdin.on("error", () => process.exit(1));
