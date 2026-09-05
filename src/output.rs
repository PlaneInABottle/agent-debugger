use serde_json::Value;
use std::io::Write;

/// Stable output envelope for agent consumption.
///
/// JSON mode (default): `{"ok":true,"command":"status","data":{...}}`
/// or `{"ok":false,"command":"step","error":"..."}`.
/// Human mode (`--human`): pretty data or `error: ...` on stderr.
///
/// Broken-pipe safe: agents paginate/truncate output (`| head`), which
/// closes stdout early. A panic there would mask the real result, so EPIPE
/// exits silently with success (the consumer already has what it needs).
pub fn emit(command: &str, result: anyhow::Result<Value>, human: bool) -> i32 {
    match result {
        Ok(data) => {
            if human {
                if data.is_null() {
                    return write_stdout(format!("ok ({command})\n"), 0);
                }
                match serde_json::to_string_pretty(&data) {
                    Ok(pretty) => write_stdout(format!("{pretty}\n"), 0),
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "error: failed to render output: {e}");
                        1
                    }
                }
            } else {
                let envelope = serde_json::json!({
                    "ok": true,
                    "command": command,
                    "data": data,
                });
                write_stdout(format!("{envelope}\n"), 0)
            }
        }
        Err(e) => {
            if human {
                let _ = writeln!(std::io::stderr(), "error: {e:#}");
            } else {
                let envelope = serde_json::json!({
                    "ok": false,
                    "command": command,
                    "error": format!("{e:#}"),
                });
                let code = write_stdout(format!("{envelope}\n"), 1);
                return code;
            }
            1
        }
    }
}

fn write_stdout(text: String, code: i32) -> i32 {
    match std::io::stdout().write_all(text.as_bytes()) {
        Ok(()) => code,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(_) => 1,
    }
}
