$ErrorActionPreference = 'Stop'
Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'rustfmt failed' }
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Clippy failed' }
    cargo clippy -p fliuno-sandbox --all-targets --no-default-features --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'standalone sandbox Clippy failed' }
    cargo test --workspace --all-features --locked
    if ($LASTEXITCODE -ne 0) { throw 'tests failed' }
    python scripts/export_schema.py --check
    if ($LASTEXITCODE -ne 0) { throw 'protocol schema is out of date' }
    python examples/demo.py
    if ($LASTEXITCODE -ne 0) { throw 'demo failed' }
    python examples/context_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'context demo failed' }
    python examples/skills_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'skills resource demo failed' }
    python examples/skills_demo.py --deny
    if ($LASTEXITCODE -ne 0) { throw 'skills resource denial demo failed' }
    python examples/skills_demo.py --switch-source
    if ($LASTEXITCODE -ne 0) { throw 'skills source binding demo failed' }
    python examples/bundle_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'Node companion demo failed' }
    python examples/bundle_demo.py --deny
    if ($LASTEXITCODE -ne 0) { throw 'Node companion denial demo failed' }
    python examples/bundle_demo.py --tamper
    if ($LASTEXITCODE -ne 0) { throw 'Node companion pin demo failed' }
    python examples/mcp_http_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'MCP HTTP JSON demo failed' }
    python examples/mcp_http_demo.py --sse
    if ($LASTEXITCODE -ne 0) { throw 'MCP HTTP SSE demo failed' }
    python examples/mcp_http_demo.py --legacy --sse
    if ($LASTEXITCODE -ne 0) { throw 'MCP HTTP legacy demo failed' }
    python examples/mcp_http_demo.py --deny
    if ($LASTEXITCODE -ne 0) { throw 'MCP HTTP denial demo failed' }
    python examples/mcp_content_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'MCP resources/prompts demo failed' }
    python examples/mcp_content_demo.py --deny
    if ($LASTEXITCODE -ne 0) { throw 'MCP resource denial demo failed' }
    python examples/mcp_content_demo.py --switch-source
    if ($LASTEXITCODE -ne 0) { throw 'MCP content registration demo failed' }
    python examples/a2a_demo.py
    if ($LASTEXITCODE -ne 0) { throw 'A2A demo failed' }
    python examples/a2a_demo.py --cancel
    if ($LASTEXITCODE -ne 0) { throw 'A2A cancellation demo failed' }
    python examples/a2a_demo.py --streaming
    if ($LASTEXITCODE -ne 0) { throw 'A2A streaming demo failed' }
    python examples/a2a_demo.py --streaming --cancel
    if ($LASTEXITCODE -ne 0) { throw 'A2A streaming cancellation demo failed' }
    if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
        python examples/bundle_demo.py --runtime python
        if ($LASTEXITCODE -ne 0) { throw 'Python companion demo failed' }
        python examples/bundle_demo.py --runtime python --deny
        if ($LASTEXITCODE -ne 0) { throw 'Python companion denial demo failed' }
        python examples/bundle_demo.py --runtime python --tamper
        if ($LASTEXITCODE -ne 0) { throw 'Python companion pin demo failed' }
        python examples/agent_demo.py
        if ($LASTEXITCODE -ne 0) { throw 'ACP agent demo failed' }
        python examples/agent_demo.py --deny
        if ($LASTEXITCODE -ne 0) { throw 'ACP agent denial demo failed' }
        python examples/agent_demo.py --workspace
        if ($LASTEXITCODE -ne 0) { throw 'ACP workspace demo failed' }
        python examples/agent_demo.py --workspace --deny
        if ($LASTEXITCODE -ne 0) { throw 'ACP workspace denial demo failed' }
    }
} finally {
    Pop-Location
}
