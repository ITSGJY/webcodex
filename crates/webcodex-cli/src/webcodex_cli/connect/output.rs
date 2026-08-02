use std::path::Path;

use super::profile::ResolvedKey;

pub(super) fn render_connect_output(
    server_url: &str,
    profile: &str,
    client_id: &str,
    runtime_project_id: &str,
    config_path: &Path,
    log_path: &Path,
    resolved_key: &ResolvedKey,
) -> String {
    let mut output = String::new();
    output.push_str("Connected to WebCodex\n\n");
    output.push_str(&format!("Server:       {}\n", server_url));
    output.push_str(&format!("MCP URL:      {}/mcp\n", server_url));
    output.push_str(&format!("Profile:      {profile}\n"));
    output.push_str(&format!("Client:       {client_id}\n"));
    output.push_str(&format!("Project:      {runtime_project_id}\n"));
    output.push_str("Runner:       running\n");
    output.push_str(&format!("Config:       {}\n", config_path.display()));
    output.push_str(&format!("Logs:         {}\n", log_path.display()));
    if resolved_key.warn_short {
        output.push_str(
            "\nWarning: the supplied shared key is short; use a long random value when possible.\n",
        );
    }
    if resolved_key.generated {
        output.push_str(&format!("\nMCP key: {}\n", resolved_key.value));
        output.push_str(
            "Copy this key now. It will not be printed in full by status commands.\n\
Use the same key in your MCP client.\n",
        );
    }
    output.push_str(&format!(
        "\nMCP URL: {}/mcp\nAuthentication: Bearer token\nToken: the same key used by this command\n",
        server_url
    ));
    output
}
