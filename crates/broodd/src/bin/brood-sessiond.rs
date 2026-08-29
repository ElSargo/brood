fn main() -> anyhow::Result<()> {
    let result = broodd::run_session_cli();
    brood_profiler::flush();
    result
}
