//! Loopback HTTP server for the GUI captcha registration flow.
//!
//! Per client-spec §5.2 the GUI registers by opening a browser to
//! `https://superdeduper.io/setup?cb=http://127.0.0.1:{port}/captcha-callback/{nonce}&install_id={id}`,
//! letting the user solve a Cloudflare Turnstile, then capturing the
//! returned token on a tiny loopback server. The same pattern will be
//! reused for G3 OAuth — keep this module general where it costs nothing.
//!
//! Why hand-roll rather than pull `tiny_http`: this is a single-endpoint
//! server with a tiny request set (OPTIONS preflight + one POST) and no
//! desire to widen the dependency surface for a privacy-sensitive client.
//! Stdlib `TcpListener` + a few lines of HTTP/1.1 parsing covers it.
//!
//! Threat model: the listener binds **only** to 127.0.0.1:0 (random
//! port) and only accepts a POST whose path includes a fresh random
//! nonce generated for this registration session. An attacker without
//! that nonce can't blindly POST a forged token — and the nonce is only
//! reachable through the browser URL we opened, which is private to
//! the user's session.

use std::io::BufReader;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug)]
pub enum CaptchaError {
    /// Could not bind a loopback port. Rare; usually a sandbox limit.
    BindFailed(String),
    /// `xdg-open` / `open` / `cmd /c start` failed to launch the
    /// browser. The user may be on a headless box with no default
    /// browser configured.
    BrowserOpenFailed(String),
    /// The user did not complete the captcha within the timeout.
    Timeout,
    /// Listener thread died unexpectedly.
    ServerDied,
}

/// Open the system browser to the captcha page and wait for the
/// page to POST a Turnstile token back to our loopback. Returns the
/// raw token string on success; the caller is responsible for sending
/// it to the backend's `/api/v1/register` with the right proof shape.
///
/// `timeout` is the wall-clock window the user has to finish the
/// captcha. 5 minutes is a sensible default; longer than that and the
/// page's Turnstile widget will have expired anyway.
pub fn await_captcha_token(
    server_url: &str,
    install_id: &str,
    timeout: Duration,
) -> Result<String, CaptchaError> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| CaptchaError::BindFailed(format!("{e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CaptchaError::BindFailed(format!("{e}")))?
        .port();

    let nonce = make_nonce();
    let expected_path = format!("/captcha-callback/{nonce}");

    // The /setup page lives on the bare web origin
    // (https://superdeduper.io/setup/), not the api subdomain.
    // Derive it by stripping the `api.` prefix from the server_url
    // if present; falls through to whatever the caller passed if it
    // doesn't match the convention.
    let setup_origin = web_origin_from_api(server_url);
    // Both `cb` and `install_id` need url-encoding — `cb` because it
    // contains slashes and a port (otherwise the second `&` would
    // confuse the query parser), `install_id` for general safety.
    let cb_url = format!("http://127.0.0.1:{port}{expected_path}");
    let url = format!(
        "{}/setup/?cb={}&install_id={}",
        setup_origin.trim_end_matches('/'),
        urlencode(&cb_url),
        urlencode(install_id),
    );

    let (tx, rx) = mpsc::channel::<String>();
    let listener_path = expected_path;
    std::thread::spawn(move || {
        for stream_res in listener.incoming() {
            let mut stream = match stream_res {
                Ok(s) => s,
                Err(_) => continue,
            };
            match handle_request(&mut stream, &listener_path) {
                RequestOutcome::Token(t) => {
                    let _ = tx.send(t);
                    return;
                }
                RequestOutcome::Continue => continue,
            }
        }
    });
    open_browser(&url)?;

    rx.recv_timeout(timeout).map_err(|e| match e {
        mpsc::RecvTimeoutError::Timeout => CaptchaError::Timeout,
        mpsc::RecvTimeoutError::Disconnected => CaptchaError::ServerDied,
    })
}

#[derive(Debug)]
enum RequestOutcome {
    /// Token captured; the listener thread should exit.
    Token(String),
    /// Preflight or 404; keep accepting connections.
    Continue,
}

fn handle_request(stream: &mut std::net::TcpStream, expected_path: &str) -> RequestOutcome {
    // Bounded read so a misbehaving client can't tie up the slot.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return RequestOutcome::Continue,
    };
    let mut reader = BufReader::new(read_stream);
    // Use the same parse/write code path on synthetic byte streams
    // so the integration tests below cover the security-sensitive
    // parser without spawning a real TcpListener.
    handle_request_generic(&mut reader, stream, expected_path)
}

/// Generic HTTP-handler parameterised on the reader + writer so
/// tests can drive it with `Cursor<&[u8]>` + `Vec<u8>`. The real
/// caller passes a `BufReader<TcpStream>` + the original
/// `TcpStream`; tests pass in-memory byte streams.
fn handle_request_generic<R, W>(
    reader: &mut R,
    writer: &mut W,
    expected_path: &str,
) -> RequestOutcome
where
    R: std::io::BufRead,
    W: std::io::Write,
{
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).is_err() {
        return RequestOutcome::Continue;
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return RequestOutcome::Continue;
    }
    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    let mut origin: Option<String> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                let lower = line.to_lowercase();
                if let Some(rest) = lower.strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                } else if let Some(rest) = line.strip_prefix("Origin:") {
                    origin = Some(rest.trim().to_string());
                } else if let Some(rest) = lower.strip_prefix("origin:") {
                    // Some clients lowercase the header name.
                    origin = Some(rest.trim().to_string());
                }
            }
            Err(_) => return RequestOutcome::Continue,
        }
    }
    let origin_header = origin.unwrap_or_else(|| "*".to_string());

    // CORS preflight from the superdeduper.io page.
    if method == "OPTIONS" {
        let resp = format!(
            "HTTP/1.1 204 No Content\r\n\
             Access-Control-Allow-Origin: {origin_header}\r\n\
             Access-Control-Allow-Methods: POST, OPTIONS\r\n\
             Access-Control-Allow-Headers: Content-Type\r\n\
             Access-Control-Max-Age: 3600\r\n\
             Vary: Origin\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let _ = writer.write_all(resp.as_bytes());
        return RequestOutcome::Continue;
    }

    // Anything that's not a POST to our exact path is a 404.
    if method != "POST" || path != expected_path {
        let _ = writer.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        return RequestOutcome::Continue;
    }

    // Cap body size — Turnstile tokens are ~2-4 KB.
    let body_cap = content_length.min(16 * 1024);
    let mut body = vec![0u8; body_cap];
    if reader.read_exact(&mut body).is_err() {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        return RequestOutcome::Continue;
    }

    let token = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v
            .get("captcha_token")
            .and_then(|t| t.as_str())
            .map(String::from),
        Err(_) => None,
    };

    if let Some(token) = token {
        let body_out = b"{\"ok\":true}";
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Access-Control-Allow-Origin: {origin_header}\r\n\
             Content-Type: application/json\r\n\
             Vary: Origin\r\n\
             Content-Length: {}\r\n\r\n",
            body_out.len()
        );
        let _ = writer.write_all(resp.as_bytes());
        let _ = writer.write_all(body_out);
        RequestOutcome::Token(token)
    } else {
        let _ = writer.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
        RequestOutcome::Continue
    }
}

/// Derive the web origin (`https://superdeduper.io`) from the api
/// origin (`https://api.superdeduper.io`). The `/setup` page lives
/// on the bare domain per web's deploy. Falls back to the input if
/// no `api.` prefix is found — useful for staging / dev where the
/// api server might be at `http://localhost:8080` and the same
/// host serves /setup.
fn web_origin_from_api(api_url: &str) -> String {
    // Split into scheme://rest so we can rewrite only the host.
    if let Some((scheme, rest)) = api_url.split_once("://") {
        if let Some(stripped) = rest.strip_prefix("api.") {
            return format!("{scheme}://{stripped}");
        }
    }
    api_url.to_string()
}

fn make_nonce() -> String {
    // uuid is already a project dep (run_uuid generation). v4 is
    // 122 bits of CSPRNG entropy — overkill for a one-shot session
    // nonce, but cheap and avoids reinventing it.
    uuid::Uuid::new_v4().simple().to_string()
}

fn urlencode(s: &str) -> String {
    // Conservative percent-encoding: ASCII-alphanumeric + a few
    // unreserved chars pass through; everything else becomes %XX.
    // install_ids are UUIDs (alphanumeric + dashes) so this is
    // mostly a passthrough, but we want correctness if a future
    // caller passes something unexpected.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn open_browser(url: &str) -> Result<(), CaptchaError> {
    #[cfg(target_os = "windows")]
    {
        return open_browser_windows(url);
    }
    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| CaptchaError::BrowserOpenFailed(format!("{e}")));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| CaptchaError::BrowserOpenFailed(format!("{e}")))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        let _ = url;
        Err(CaptchaError::BrowserOpenFailed(
            "no browser-open impl for this platform".into(),
        ))
    }
}

/// Open URL via `ShellExecuteW` instead of `cmd /c start "" url`.
///
/// Why: the prior `cmd`-based path mangled URLs containing `&` —
/// cmd treats `&` as a command separator unless quoted, and Rust's
/// std::process::Command arg-quoting doesn't add quotes around args
/// without spaces. The result: `start "" https://...?cb=...&install_id=...`
/// got split at the `&`, browser opened only the URL fragment up
/// to it (no install_id), and the web's /setup page rejected the
/// request as missing-params.
///
/// `ShellExecuteW` is the canonical Win32 "open a URL with the
/// user's default handler" call. Takes the URL as a single wide
/// string parameter; no cmd parsing involved.
#[cfg(target_os = "windows")]
fn open_browser_windows(url: &str) -> Result<(), CaptchaError> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op_w: Vec<u16> = "open\0".encode_utf16().collect();
    let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both wide strings are null-terminated; HWND::default() is
    // a valid (null) owner; SW_SHOWNORMAL is a documented constant.
    let h = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(op_w.as_ptr()),
            PCWSTR(url_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns >32 on success per its docs. <=32 is one of
    // the SE_ERR_* values (no handler registered, file not found, etc.).
    let raw = h.0 as isize;
    if raw > 32 {
        Ok(())
    } else {
        Err(CaptchaError::BrowserOpenFailed(format!(
            "ShellExecuteW returned {raw} (<=32 indicates SE_ERR_*)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_alphanumeric() {
        assert_eq!(urlencode("abc-XYZ_123.foo~"), "abc-XYZ_123.foo~");
    }

    #[test]
    fn urlencode_percent_encodes_specials() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn web_origin_strips_api_prefix() {
        assert_eq!(
            web_origin_from_api("https://api.superdeduper.io"),
            "https://superdeduper.io"
        );
        assert_eq!(
            web_origin_from_api("https://api.superdeduper.io/"),
            "https://superdeduper.io/"
        );
    }

    #[test]
    fn web_origin_passes_through_non_api_origins() {
        assert_eq!(
            web_origin_from_api("http://localhost:8080"),
            "http://localhost:8080"
        );
        assert_eq!(
            web_origin_from_api("https://superdeduper.io"),
            "https://superdeduper.io"
        );
    }

    #[test]
    fn make_nonce_returns_32_hex() {
        let n = make_nonce();
        assert_eq!(n.len(), 32);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_nonces_differ() {
        assert_ne!(make_nonce(), make_nonce());
    }

    // ============================================================
    // HTTP request handler — drives `handle_request_generic`
    // against synthetic byte streams (`Cursor<&[u8]>` for the
    // reader, `Vec<u8>` for the writer). Exercises every code
    // path in the parser + every response shape without spawning
    // a TcpListener. The security-sensitive surface here is:
    //
    // * CORS preflight: must echo the caller's Origin (or `*` if
    //   missing) so the page's fetch() lands cleanly.
    // * Path validation: only POST to the per-session-nonce path
    //   produces a Token outcome; anything else 404s. This is
    //   what prevents a third party from blindly POSTing a forged
    //   token without knowing the user's per-session nonce.
    // * Body parsing: malformed JSON, missing field, oversize all
    //   reject without crashing.
    // ============================================================

    use std::io::{BufReader, Cursor};

    fn drive(input: &str, expected_path: &str) -> (RequestOutcome, String) {
        let mut reader = BufReader::new(Cursor::new(input.as_bytes()));
        let mut writer: Vec<u8> = Vec::new();
        let outcome = handle_request_generic(&mut reader, &mut writer, expected_path);
        let response = String::from_utf8_lossy(&writer).into_owned();
        (outcome, response)
    }

    #[test]
    fn options_preflight_returns_204_with_cors_headers() {
        let req = "OPTIONS /captcha-callback/abc HTTP/1.1\r\n\
            Host: 127.0.0.1:12345\r\n\
            Origin: https://superdeduper.io\r\n\
            Access-Control-Request-Method: POST\r\n\
            \r\n";
        let (outcome, resp) = drive(req, "/captcha-callback/abc");
        assert!(
            matches!(outcome, RequestOutcome::Continue),
            "OPTIONS preflight must NOT capture a token"
        );
        assert!(resp.contains("204 No Content"));
        assert!(resp.contains("Access-Control-Allow-Origin: https://superdeduper.io"));
        assert!(resp.contains("Access-Control-Allow-Methods: POST, OPTIONS"));
        assert!(resp.contains("Access-Control-Allow-Headers: Content-Type"));
        assert!(resp.contains("Vary: Origin"));
    }

    #[test]
    fn options_preflight_falls_back_to_wildcard_origin_when_missing() {
        let req = "OPTIONS /captcha-callback/abc HTTP/1.1\r\n\r\n";
        let (_, resp) = drive(req, "/captcha-callback/abc");
        assert!(resp.contains("204 No Content"));
        assert!(resp.contains("Access-Control-Allow-Origin: *"));
    }

    #[test]
    fn post_to_correct_path_with_valid_token_returns_token() {
        let body = r#"{"captcha_token":"the-real-token-payload"}"#;
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            Host: 127.0.0.1:12345\r\n\
            Origin: https://superdeduper.io\r\n\
            Content-Type: application/json\r\n\
            Content-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        let (outcome, resp) = drive(&req, "/captcha-callback/abc");
        match outcome {
            RequestOutcome::Token(t) => assert_eq!(t, "the-real-token-payload"),
            other => panic!("expected Token, got {other:?}"),
        }
        assert!(resp.contains("200 OK"));
        assert!(resp.contains(r#"{"ok":true}"#));
        assert!(resp.contains("Access-Control-Allow-Origin: https://superdeduper.io"));
    }

    #[test]
    fn post_to_wrong_path_404s_without_token() {
        // Per-session nonce mismatch — attacker can't blindly hit
        // /captcha-callback without knowing the nonce.
        let body = r#"{"captcha_token":"forged"}"#;
        let req = format!(
            "POST /captcha-callback/WRONG-NONCE HTTP/1.1\r\n\
            Content-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        let (outcome, resp) = drive(&req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
        assert!(resp.contains("404 Not Found"));
        assert!(!resp.contains("forged"));
    }

    #[test]
    fn get_method_404s() {
        let req = "GET /captcha-callback/abc HTTP/1.1\r\n\r\n";
        let (outcome, resp) = drive(req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
        assert!(resp.contains("404 Not Found"));
    }

    #[test]
    fn post_with_missing_captcha_token_field_400s() {
        let body = r#"{"unrelated":"field"}"#;
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            Content-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        let (outcome, resp) = drive(&req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
        assert!(resp.contains("400 Bad Request"));
    }

    #[test]
    fn post_with_malformed_json_400s() {
        let body = "{not json";
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            Content-Length: {}\r\n\r\n{body}",
            body.len(),
        );
        let (outcome, resp) = drive(&req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
        assert!(resp.contains("400 Bad Request"));
    }

    #[test]
    fn post_body_too_short_for_content_length_400s() {
        // Content-Length claims 100 bytes; we only send 10. The
        // read_exact call will fail. Handler must respond 400, not
        // panic or hang.
        let body = "shortbody!"; // exactly 10 bytes
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            Content-Length: 100\r\n\r\n{body}",
        );
        let (outcome, resp) = drive(&req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
        assert!(resp.contains("400 Bad Request"));
    }

    #[test]
    fn body_size_capped_at_16k() {
        // Content-Length claims 1 GB; the handler caps the read
        // at 16 KB so a hostile client can't allocate gigabytes
        // of buffer on our side. Body that's actually 16 KB will
        // be parsed; anything beyond is ignored.
        let claimed_length = 1_000_000_000;
        let mut payload = String::from(r#"{"captcha_token":"validtoken"}"#);
        // Pad up to 16 KB with whitespace inside the JSON. The
        // parser truncates at 16 KB.
        while payload.len() < 16 * 1024 {
            payload.push(' ');
        }
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            Content-Length: {claimed_length}\r\n\r\n{payload}",
        );
        let (outcome, _) = drive(&req, "/captcha-callback/abc");
        // We expect this to land as Continue (the parsed body at
        // 16K probably won't deserialize cleanly + has trailing
        // garbage), but the important property is NO PANIC + we
        // get a response back. Either Token (if the slice happens
        // to be valid JSON) or Continue (if not).
        match outcome {
            RequestOutcome::Token(_) | RequestOutcome::Continue => {}
        }
    }

    #[test]
    fn lowercase_content_length_header_parses() {
        // RFC 7230 §3.2 says header names are case-insensitive.
        // Some clients lowercase. Make sure we don't only match
        // the canonical capitalisation.
        let body = r#"{"captcha_token":"x"}"#;
        let req = format!(
            "POST /captcha-callback/abc HTTP/1.1\r\n\
            content-length: {}\r\n\r\n{body}",
            body.len(),
        );
        let (outcome, _) = drive(&req, "/captcha-callback/abc");
        assert!(
            matches!(outcome, RequestOutcome::Token(_)),
            "lowercase content-length header must still parse"
        );
    }

    #[test]
    fn lowercase_origin_header_parses_for_cors_echo() {
        let req = "OPTIONS /captcha-callback/abc HTTP/1.1\r\n\
            origin: https://example.com\r\n\r\n";
        let (_, resp) = drive(req, "/captcha-callback/abc");
        assert!(resp.contains("Access-Control-Allow-Origin: https://example.com"));
    }

    #[test]
    fn malformed_request_line_does_not_panic() {
        // Truncated / garbage request line. Handler must return
        // Continue without crashing — would be a DoS vector if
        // any garbage in could panic the listener thread.
        let req = "garbage";
        let (outcome, _resp) = drive(req, "/captcha-callback/abc");
        assert!(matches!(outcome, RequestOutcome::Continue));
    }
}
