//! Test-only real stdio peer; ordinary application builds do not include it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    fliuno_acp::fixture::run_stdio()
}
