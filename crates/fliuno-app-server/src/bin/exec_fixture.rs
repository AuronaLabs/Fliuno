fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("terminal-fixture") => return fliuno_terminal::fixture::run(),
        Some("terminal-descendant") => fliuno_terminal::fixture::descendant(),
        _ => {}
    }
    fliuno_exec::fixture::main()
}
