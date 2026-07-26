// Standalone newline-delimited JSON-RPC MCP fixture. Tests compile this file
// directly with rustc; it is not a production binary target.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

fn main() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let scenario = args.first().map(String::as_str).unwrap_or("normal");
    let marker = args.get(1).map(Path::new);
    append(marker, "start\n")?;
    let mut reader = BufReader::new(io::stdin().lock());
    let mut writer = io::stdout().lock();
    loop {
        let mut body = String::new();
        if reader.read_line(&mut body)? == 0 {
            return Ok(());
        }
        append(marker, &format!("request:{}\n", body.trim_end()))?;
        let method = string_field(&body, "method");
        let id = u64_field(&body, "id").unwrap_or(0);
        match method.as_deref() {
            Some("initialize") => send(
                &mut writer,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake","version":"Claude Fake 1.2.3"}}}}}}"#
                ),
            )?,
            Some("notifications/initialized") => {}
            Some("tools/list") => send(
                &mut writer,
                &format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"fake_search","inputSchema":{{"type":"object","properties":{{"pattern":{{"type":"string"}},"path":{{"type":"string"}},"output_mode":{{"type":"string"}},"head_limit":{{"type":"integer"}},"-n":{{"type":"boolean"}},"-B":{{"type":"integer"}},"-A":{{"type":"integer"}}}}}}}},{{"name":"fake_edit","inputSchema":{{"type":"object","properties":{{"file_path":{{"type":"string"}},"old_string":{{"type":"string"}},"new_string":{{"type":"string"}}}}}}}}]}}}}"#
                ),
            )?,
            Some("tools/call") => match scenario {
                "invalid_json" => send(&mut writer, "{invalid")?,
                "timeout" => thread::sleep(Duration::from_secs(5)),
                "oversized" => {
                    let text = "x".repeat(1024 * 1024 + 100);
                    send(&mut writer, &tool_result(id, &text))?;
                }
                "exit" => return Ok(()),
                "restart_once" if !marker_contains(marker, "crashed") => {
                    append(marker, "crashed\n")?;
                    return Ok(());
                }
                _ => {
                    if scenario == "delayed" {
                        thread::sleep(Duration::from_millis(250));
                    }
                    if scenario == "unknown_id" {
                        send(
                            &mut writer,
                            r#"{"jsonrpc":"2.0","id":999999,"result":{"ignored":true}}"#,
                        )?;
                    }
                    if scenario == "server_request" {
                        send(
                            &mut writer,
                            r#"{"jsonrpc":"2.0","id":"server-request-1","method":"sampling/createMessage","params":{}}"#,
                        )?;
                        let mut response = String::new();
                        reader.read_line(&mut response)?;
                        if !response.contains(r#""id":"server-request-1""#)
                            || !response.contains(r#""code":-32601"#)
                            || !response.contains(r#""message":"Method not found""#)
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "client did not reject unsupported server request",
                            ));
                        }
                        append(marker, "server_request_error_received\n")?;
                    }
                    let name = string_field(&body, "name").unwrap_or_default();
                    let text = match name.as_str() {
                        "fake_search" => format!(
                            "{}/src/lib.rs:2:needle",
                            env::current_dir()?.display()
                        ),
                        "fake_edit" => {
                            let path = env::current_dir()?.join("edit.txt");
                            let before = fs::read_to_string(&path)?;
                            fs::write(path, before.replacen("before", "after", 1))?;
                            "edited".to_string()
                        }
                        _ => "unknown tool".to_string(),
                    };
                    send(&mut writer, &tool_result(id, &text))?;
                }
            },
            _ => {}
        }
    }
}

fn send(writer: &mut impl Write, body: &str) -> io::Result<()> {
    writer.write_all(body.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn tool_result(id: u64, text: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{}"}}],"isError":false}}}}"#,
        escape(text)
    )
}

fn u64_field(body: &str, field: &str) -> Option<u64> {
    let after = body.split_once(&format!(r#""{field}""#))?.1;
    let value = after.split_once(':')?.1.trim_start();
    value
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

fn string_field(body: &str, field: &str) -> Option<String> {
    let after = body.split_once(&format!(r#""{field}""#))?.1;
    let value = after.split_once(':')?.1.trim_start().strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string())
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn append(marker: Option<&Path>, text: &str) -> io::Result<()> {
    if let Some(marker) = marker {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(marker)?
            .write_all(text.as_bytes())?;
    }
    Ok(())
}

fn marker_contains(marker: Option<&Path>, needle: &str) -> bool {
    marker
        .and_then(|path| fs::read_to_string(path).ok())
        .is_some_and(|text| text.contains(needle))
}
