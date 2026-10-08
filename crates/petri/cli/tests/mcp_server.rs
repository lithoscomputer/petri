//! The scripted MCP server must accept a handshake across HTTP connections.

use std::path::Path;
use std::process::Command;

#[test]
fn scripted_http_server_handles_overlapping_connections() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/test_mcp_server.py");
    let output = Command::new("python3")
        .arg(script)
        .env_remove("MCP_TEST_LOG")
        .env_remove("MCP_TEST_TRACE")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("python3 runs the scripted server's connection tests");
    assert!(
        output.status.success(),
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
