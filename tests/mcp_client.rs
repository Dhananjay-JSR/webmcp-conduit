//! End-to-end conformance: a real MCP client, over both transports.
//!
//! Everything else in the suite tests conduit's own behaviour. This tests
//! whether an MCP client can actually talk to it, which is a different question
//! and not one `curl` can answer — a hand-built request proves the server
//! answers *that* request, not that a client library's handshake succeeds.
//!
//! The gap is not hypothetical. Two defaults in the SDK's HTTP transport were
//! wrong for conduit and neither showed up in a curl session: an SSE priming
//! event with an empty data field, which clients that parse every `data:` line
//! as JSON choke on, and a `Host` allowlist of loopback only, which would have
//! rejected every request to a deployed server.
//!
//! The client here is the SDK's own, so these are the same code paths a real
//! host uses.

use rmcp::model::CallToolRequestParams;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{serve_client, ServiceExt};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// A page with one tool whose result depends on its argument, so a passing
/// test means arguments arrived and a result came back — not merely that
/// something was listed.
const PAGE: &str = r#"<!doctype html><html><body><script>
document.modelContext.registerTool({
  name: "greet",
  description: "Greet someone by name",
  inputSchema: {
    type: "object",
    properties: { name: { type: "string" } },
    required: ["name"],
  },
  async execute({ name }) { return "hello " + name; },
});
</script></body></html>"#;

fn fixture_path() -> std::path::PathBuf {
    // Named per process so concurrent test binaries cannot collide.
    let path = std::env::temp_dir().join(format!("conduit-mcp-{}.html", std::process::id()));
    std::fs::write(&path, PAGE).expect("writing the fixture");
    path
}

/// Serve the fixture over HTTP.
///
/// `--site` takes an http URL, not a file, because a hosted conduit fetches
/// pages rather than reading them off its own disk. So the HTTP test needs a
/// real origin, and a socket is cheaper than a dependency.
async fn serve_fixture() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding the fixture server");
    let addr = listener.local_addr().expect("fixture address");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // Read the request line and headers, and no further: the body
                // is irrelevant and waiting for EOF would hang on keep-alive.
                let mut seen = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read_exact(&mut byte).await.is_ok() {
                    seen.push(byte[0]);
                    if seen.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}/")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_client_can_drive_a_page_over_stdio() {
    let fixture = fixture_path();

    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_conduit"));
    command.arg("serve").arg(&fixture);
    // Kept out of the developer's real session store.
    command.env(
        "CONDUIT_SESSION_DIR",
        std::env::temp_dir().join("conduit-test-sessions"),
    );

    let client = serve_client(
        (),
        TokioChildProcess::new(command).expect("spawning conduit"),
    )
    .await
    .expect("the client should complete the MCP handshake over stdio");

    let info = client
        .peer_info()
        .expect("the server should describe itself");
    assert_eq!(
        info.server_info.as_ref().map(|i| i.name.as_str()),
        Some("webmcp-conduit")
    );
    assert!(
        info.instructions
            .as_deref()
            .unwrap_or_default()
            .contains("WebMCP"),
        "instructions should say where the tools came from"
    );

    let tools = client.list_all_tools().await.expect("listing tools");
    let greet = tools
        .iter()
        .find(|t| t.name.ends_with("greet"))
        .unwrap_or_else(|| panic!("expected a greet tool, got {:?}", names(&tools)));

    let result = client
        .call_tool(
            CallToolRequestParams::new(greet.name.clone()).with_arguments(
                serde_json::json!({"name": "ada"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("calling the tool");

    assert_eq!(text_of(&result), "hello ada");

    client.cancel().await.expect("shutting the client down");
    let _ = std::fs::remove_file(&fixture);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_client_can_drive_a_page_over_http() {
    let site = serve_fixture().await;

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_conduit"))
        .args([
            "serve",
            "--transport",
            "http",
            // Port 0: the OS picks, and conduit reports what it got. A fixed
            // port would make two test runs on one machine flaky.
            "--bind",
            "127.0.0.1:0",
            "--site",
            &format!("fixture={site}"),
            // No session, so the test leaves nothing behind on disk.
            "--stateless",
        ])
        .env(
            "CONDUIT_SESSION_DIR",
            std::env::temp_dir().join("conduit-test-sessions"),
        )
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning conduit over http");

    let stderr = child.stderr.take().expect("conduit stderr");
    let address = tokio::time::timeout(Duration::from_secs(30), read_address(stderr))
        .await
        .expect("conduit should report its address")
        .expect("conduit should report its address");

    let transport = StreamableHttpClientTransport::from_uri(format!("http://{address}/fixture"));
    let client = ().serve(transport).await.expect(
        "the client should complete the MCP handshake over Streamable HTTP. \
         A failure here is an interoperability bug, not a conduit bug: the \
         server answered, but not in a way a client library accepts.",
    );

    let info = client
        .peer_info()
        .expect("the server should describe itself");
    assert_eq!(
        info.server_info.as_ref().map(|i| i.name.as_str()),
        Some("webmcp-conduit")
    );

    let tools = client.list_all_tools().await.expect("listing tools");
    let greet = tools
        .iter()
        .find(|t| t.name.ends_with("greet"))
        .unwrap_or_else(|| panic!("expected a greet tool, got {:?}", names(&tools)));

    let result = client
        .call_tool(
            CallToolRequestParams::new(greet.name.clone()).with_arguments(
                serde_json::json!({"name": "grace"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .expect("calling the tool");

    assert_eq!(text_of(&result), "hello grace");

    client.cancel().await.expect("shutting the client down");
    let _ = child.kill().await;
}

/// What a strict client sees on the wire.
///
/// The two tests above use the SDK's own client, which is the right check for
/// "does a host connect" and the wrong one for "is the framing acceptable to
/// everyone". A client and server from one implementation agree with each other
/// by construction: flipping the transport back to legacy session mode, the bug
/// Postman reported, leaves both of them passing.
///
/// So this reads the bytes instead. It asserts the property that actually broke:
/// no frame that a client will try to parse as JSON may be empty.
#[tokio::test(flavor = "multi_thread")]
async fn no_response_frame_is_empty_for_a_strict_parser() {
    let site = serve_fixture().await;

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_conduit"))
        .args([
            "serve",
            "--transport",
            "http",
            "--bind",
            "127.0.0.1:0",
            "--site",
            &format!("fixture={site}"),
            "--stateless",
        ])
        .env(
            "CONDUIT_SESSION_DIR",
            std::env::temp_dir().join("conduit-test-sessions"),
        )
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning conduit over http");

    let stderr = child.stderr.take().expect("conduit stderr");
    let address = tokio::time::timeout(Duration::from_secs(30), read_address(stderr))
        .await
        .expect("conduit should report its address")
        .expect("conduit should report its address");

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"strict","version":"1"}}}"#;
    let raw = post(&address, "/fixture", body).await;

    let (headers, payload) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header/body split in: {raw}"));

    assert!(
        headers.starts_with("HTTP/1.1 200"),
        "initialize should succeed, got: {headers}"
    );

    if headers.to_ascii_lowercase().contains("text/event-stream") {
        // SSE is permitted by the spec, but every data field a client will
        // parse has to be parseable. An empty one is what produced Postman's
        // "Invalid message syntax".
        for (i, line) in payload.lines().enumerate() {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            assert!(
                !data.is_empty(),
                "SSE frame {i} has an empty data field; a client that parses \
                 every data line as JSON fails here before it sees a message"
            );
            serde_json::from_str::<serde_json::Value>(data)
                .unwrap_or_else(|e| panic!("SSE frame {i} is not JSON ({e}): {data}"));
        }
    } else {
        assert!(
            headers.to_ascii_lowercase().contains("application/json"),
            "a response must be application/json or text/event-stream, got: {headers}"
        );
        serde_json::from_str::<serde_json::Value>(payload.trim())
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {payload}"));
    }

    let _ = child.kill().await;
}

/// A POST written by hand, so the bytes on the wire are the bytes asserted on.
///
/// Deliberately more forgiving than it looks, because this failed once on a CI
/// runner and the message — `reading the response` — said nothing about what
/// had arrived. Two changes came out of that:
///
/// - a reset or a truncated read is only an error when nothing useful arrived.
///   `Connection: close` means the server hangs up after answering, and a peer
///   that closes hard enough turns the final read into `ConnectionReset`
///   instead of a clean EOF. With a complete response already buffered, that
///   distinction is noise.
/// - every failure carries the bytes received so far. A test that fails on a
///   machine you cannot attach to has to explain itself in the message.
async fn post(address: &str, path: &str, body: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = connect(address).await;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("sending the request");

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = tokio::time::timeout(Duration::from_secs(60), stream.read(&mut chunk)).await;

        match read {
            Ok(Ok(0)) => break, // Clean EOF: the server closed, as asked.
            Ok(Ok(n)) => raw.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => {
                if complete(&raw) {
                    break;
                }
                panic!(
                    "reading the response failed ({e}) with {} byte(s) received:\n{}",
                    raw.len(),
                    String::from_utf8_lossy(&raw)
                );
            }
            Err(_) => panic!(
                "conduit did not finish answering within 60s; {} byte(s) received:\n{}",
                raw.len(),
                String::from_utf8_lossy(&raw)
            ),
        }

        // `Connection: close` should give a clean EOF, but a peer that resets
        // instead would otherwise cost the whole timeout for a response that
        // already arrived in full.
        if complete(&raw) {
            break;
        }
    }

    assert!(
        !raw.is_empty(),
        "conduit closed the connection without sending anything"
    );

    String::from_utf8_lossy(&raw).into_owned()
}

/// Whether the buffer holds a whole HTTP response: headers, plus a body as long
/// as `Content-Length` promised.
fn complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };

    let declared = headers.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length:")?
            .trim()
            .parse::<usize>()
            .ok()
    });

    match declared {
        Some(length) => body.len() >= length,
        // No Content-Length means the body is delimited by the close, so the
        // response is only complete when the connection ends.
        None => false,
    }
}

/// The server is listening by the time this runs — it said so on stderr — but a
/// runner under load can still refuse the first connection.
async fn connect(address: &str) -> tokio::net::TcpStream {
    let mut last = None;
    for attempt in 0..20 {
        match tokio::net::TcpStream::connect(address).await {
            Ok(stream) => return stream,
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    panic!("could not connect to conduit at {address}: {last:?}");
}

/// Wait for the line conduit prints once it is listening, and take the address
/// from it. Polling the port instead would race the bind.
async fn read_address(stderr: tokio::process::ChildStderr) -> Option<String> {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.strip_prefix("conduit: listening on http://") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn names(tools: &[rmcp::model::Tool]) -> Vec<String> {
    tools.iter().map(|t| t.name.to_string()).collect()
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    assert_ne!(
        result.is_error,
        Some(true),
        "the tool reported an error: {:?}",
        result.content
    );
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("")
}
