// Standalone newline-delimited stdio MCP fixture. Tests compile this file
// directly with rustc so no closed-source or installed MCP server is needed.

use std::env;
use std::fs::OpenOptions;
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
    let mut calls = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let method = string_field(&line, "method").unwrap_or_default();
        let id = u64_field(&line, "id").unwrap_or(0);
        match method.as_str() {
            "initialize" => {
                append(marker, "initialize\n")?;
                if scenario == "init_crash" {
                    return Ok(());
                }
                if scenario == "init_timeout" {
                    thread::sleep(Duration::from_secs(3));
                }
                send(
                    &mut writer,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake-bridge","version":"1"}}}}}}"#
                    ),
                )?;
            }
            "notifications/initialized" => append(marker, "initialized\n")?,
            "tools/list" => {
                append(marker, "list\n")?;
                if scenario == "oversized_message" {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"padding":"{}","tools":[]}}}}"#,
                            "x".repeat(1024 * 1024)
                        ),
                    )?;
                    continue;
                }
                if scenario == "malformed" {
                    send(&mut writer, "{not-json")?;
                    continue;
                }
                if scenario == "unknown_id" {
                    send(
                        &mut writer,
                        r#"{"jsonrpc":"2.0","id":999999,"result":{"tools":[]}}"#,
                    )?;
                    continue;
                }
                let description = if scenario == "bad_tools" {
                    "x".repeat(5 * 1024)
                } else {
                    "Persistent fake echo".to_string()
                };
                send(
                    &mut writer,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","description":"{description}","inputSchema":{{"type":"object","properties":{{"value":{{"type":"string"}}}}}}}}]}}}}"#
                    ),
                )?;
                if scenario == "duplicate_id" {
                    send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[]}}}}"#
                        ),
                    )?;
                }
            }
            "tools/call" => {
                calls += 1;
                append(marker, "call\n")?;
                match scenario {
                    "crash" => return Ok(()),
                    "timeout" => {
                        thread::sleep(Duration::from_secs(3));
                    }
                    "bad_result" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"image","data":"AA==","mimeType":"image/png"}}]}}}}"#
                        ),
                    )?,
                    "oversized_result" => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
                            "x".repeat(70 * 1024)
                        ),
                    )?,
                    _ => send(
                        &mut writer,
                        &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"call-{calls}"}}],"structuredContent":{{"call":{calls}}},"isError":false}}}}"#
                        ),
                    )?,
                }
            }
            _ => {}
        }
    }
}

fn send(writer: &mut impl Write, message: &str) -> io::Result<()> {
    writer.write_all(message.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn append(path: Option<&Path>, value: &str) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(value.as_bytes())
}

fn string_field(body: &str, field: &str) -> Option<String> {
    let prefix = format!(r#""{field}":"#);
    let start = body.find(&prefix)? + prefix.len();
    let value = body.get(start..)?.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn u64_field(body: &str, field: &str) -> Option<u64> {
    let prefix = format!(r#""{field}":"#);
    let start = body.find(&prefix)? + prefix.len();
    let digits = body
        .get(start..)?
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}
