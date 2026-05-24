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

use std::io::{BufRead, BufReader, Read, Write};
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
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| CaptchaError::BindFailed(format!("{e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CaptchaError::BindFailed(format!("{e}")))?
        .port();

    let nonce = make_nonce();
    let expected_path = format!("/captcha-callback/{nonce}");

    let url = format!(
        "{}/setup?cb=http://127.0.0.1:{}{}&install_id={}",
        server_url.trim_end_matches('/'),
        port,
        expected_path,
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
        let _ = stream.write_all(resp.as_bytes());
        return RequestOutcome::Continue;
    }

    // Anything that's not a POST to our exact path is a 404.
    if method != "POST" || path != expected_path {
        let _ = stream.write_all(
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        );
        return RequestOutcome::Continue;
    }

    // Cap body size — Turnstile tokens are ~2-4 KB.
    let body_cap = content_length.min(16 * 1024);
    let mut body = vec![0u8; body_cap];
    if reader.read_exact(&mut body).is_err() {
        let _ = stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        );
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
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.write_all(body_out);
        RequestOutcome::Token(token)
    } else {
        let _ = stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        );
        RequestOutcome::Continue
    }
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
    let result = if cfg!(target_os = "windows") {
        // `cmd /c start "" url`. The empty quoted arg is `start`'s
        // window-title slot — without it, `start` would interpret
        // the URL itself as the title.
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    result
        .map(|_| ())
        .map_err(|e| CaptchaError::BrowserOpenFailed(format!("{e}")))
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
    fn make_nonce_returns_32_hex() {
        let n = make_nonce();
        assert_eq!(n.len(), 32);
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_nonces_differ() {
        assert_ne!(make_nonce(), make_nonce());
    }
}
