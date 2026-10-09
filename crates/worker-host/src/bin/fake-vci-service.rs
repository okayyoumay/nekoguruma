//! Test double for the stdio control channel of the worker service binaries.
//! Not shipped. The library name selects the behavior: `startup-error`, `never-running`,
//! `ignore-stop` (a stop request is never answered and the process never exits), anything else starts normally.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let library = arg.split_once(':').map(|(_, rest)| rest.to_owned());
    let mode = library.clone().unwrap_or_default();
    let mut key: Option<String> = None;
    let mut polls = 0;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "not json: startup noise").unwrap();
    out.flush().unwrap();

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (result, exit) = match method {
            "set_auth_key" => {
                key = req["params"]["key"].as_str().map(str::to_owned);
                (json!({}), false)
            }
            "get_status" => {
                polls += 1;
                let status = match mode.as_str() {
                    "startup-error" => {
                        json!({"running": false, "library_name": library, "endpoints": null, "error": "library not found"})
                    }
                    "never-running" => {
                        json!({"running": false, "library_name": library, "endpoints": null})
                    }
                    _ if polls < 2 => {
                        json!({"running": false, "library_name": library, "endpoints": null})
                    }
                    _ => {
                        json!({"running": true, "library_name": library, "endpoints": ["127.0.0.1:50051", "[::1]:50051"]})
                    }
                };
                (status, false)
            }
            "echo_auth_key" => (json!({"key": key, "library": library}), false),
            "echo_long_size" => (
                json!({"long_size": std::env::var("NGR_J2534_LONG_SIZE").ok()}),
                false,
            ),
            "stop" if mode == "ignore-stop" => loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
            "stop" => (json!({"stopped": true}), true),
            _ => {
                writeln!(out, "{}", json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})).unwrap();
                out.flush().unwrap();
                continue;
            }
        };
        writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        )
        .unwrap();
        out.flush().unwrap();
        if exit {
            break;
        }
    }
}
