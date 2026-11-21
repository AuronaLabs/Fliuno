//! Test-only real stdio peer; ordinary application builds do not include it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--host-login-fixture") {
        use std::io::{self, Write};
        if std::env::var("FLIUNO_TEST_LOGIN_ALLOWED").as_deref() != Ok("explicit-host-marker")
            || std::env::var_os("FLIUNO_TEST_LOGIN_BLOCKED").is_some()
        {
            return Err("incorrect login environment".into());
        }
        println!("host-fixture-awaiting-input");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if line.trim_end() != "fixture-private-input" {
            return Err("incorrect login input".into());
        }
        println!("host-fixture-awaiting-confirmation");
        io::stdout().flush()?;
        line.clear();
        io::stdin().read_line(&mut line)?;
        if line.trim_end() != "confirm-once" {
            return Err("input was repeated or incorrect".into());
        }
        std::fs::write("fixture-login-complete", b"")?;
        println!("fixture-private-output");
        io::stdout().flush()?;
        return Ok(());
    }
    fliuno_acp::fixture::run_stdio()
}
