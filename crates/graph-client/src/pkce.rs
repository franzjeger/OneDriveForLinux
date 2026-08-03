//! Building blocks for the OAuth2 authorization code flow with PKCE
//! (RFC 7636): code verifier/challenge, a one-shot loopback redirect
//! listener, and redirect query parsing.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// A cryptographically random, URL-safe token — used for the PKCE code
/// verifier and the CSRF `state` parameter.
pub fn random_token() -> Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open /dev/urandom")?
        .read_exact(&mut buf)
        .context("read /dev/urandom")?;
    Ok(B64.encode(buf))
}

/// S256 code challenge for a verifier: base64url(sha256(verifier)).
pub fn code_challenge(verifier: &str) -> String {
    B64.encode(Sha256::digest(verifier.as_bytes()))
}

/// Percent-encode a value for use in a query string.
pub fn encode_param(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Outcome of the browser redirect.
pub enum Redirect {
    Code(String),
    /// Azure reported an error (e.g. access_denied): (code, description)
    Error(String, String),
}

/// Extract `code`/`error` from a redirect request line, verifying `state`.
///
/// `request_line` looks like `GET /?code=abc&state=xyz HTTP/1.1`.
pub fn parse_redirect(request_line: &str, expected_state: &str) -> Result<Redirect> {
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed request line"))?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for pair in query.split('&') {
        let Some((key, raw)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_encoding::percent_decode_str(&raw.replace('+', " "))
            .decode_utf8_lossy()
            .to_string();
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => error_description = Some(value),
            _ => {}
        }
    }

    // The state check guards against a cross-site request landing here.
    if state.as_deref() != Some(expected_state) {
        bail!("sign-in response did not match this request (state mismatch)");
    }
    if let Some(err) = error {
        return Ok(Redirect::Error(
            err,
            error_description.unwrap_or_else(|| "no description".into()),
        ));
    }
    code.map(Redirect::Code)
        .ok_or_else(|| anyhow!("sign-in response contained no authorization code"))
}

/// Page shown in the browser once the redirect arrives.
fn result_page(heading: &str, detail: &str, accent: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>OneDrive for Linux</title>\
         <style>body{{font:16px/1.6 system-ui,sans-serif;background:#12181f;color:#e8edf3;\
         display:grid;place-items:center;height:100vh;margin:0}}\
         .c{{text-align:center;max-width:34ch}}h1{{font-size:20px;margin:.4em 0;color:{accent}}}\
         p{{color:#a9b6c5;font-size:14px}}</style>\
         <div class=c><div style=\"font-size:40px\">☁</div><h1>{heading}</h1><p>{detail}</p></div>"
    )
}

/// Block until the browser hits the loopback redirect, then return the code.
/// Runs the accept loop non-blocking so the deadline is always honoured.
pub fn wait_for_redirect(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener
        .set_nonblocking(true)
        .context("set listener non-blocking")?;
    let deadline = Instant::now() + timeout;

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok();
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

                let mut line = String::new();
                BufReader::new(stream.try_clone().context("clone stream")?)
                    .read_line(&mut line)
                    .context("read redirect request")?;

                let outcome = parse_redirect(&line, expected_state);
                let body = match &outcome {
                    Ok(Redirect::Code(_)) => result_page(
                        "You're signed in",
                        "You can close this tab — syncing starts automatically.",
                        "#57b183",
                    ),
                    Ok(Redirect::Error(code, desc)) => {
                        result_page("Sign-in failed", &format!("{code}: {desc}"), "#d0716a")
                    }
                    Err(e) => result_page("Sign-in failed", &e.to_string(), "#d0716a"),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.flush();

                return match outcome? {
                    Redirect::Code(code) => Ok(code),
                    Redirect::Error(code, desc) => {
                        bail!("Microsoft rejected the sign-in ({code}): {desc}")
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the browser sign-in to complete");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e).context("accept redirect connection"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_test_vector() {
        // RFC 7636 Appendix B.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_tokens_are_url_safe_and_unique() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "verifier must be at least 43 chars");
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn parses_authorization_code() {
        let line = "GET /?code=abc123&state=xyz HTTP/1.1";
        match parse_redirect(line, "xyz").unwrap() {
            Redirect::Code(code) => assert_eq!(code, "abc123"),
            _ => panic!("expected a code"),
        }
    }

    #[test]
    fn rejects_state_mismatch() {
        let line = "GET /?code=abc&state=wrong HTTP/1.1";
        assert!(parse_redirect(line, "expected").is_err());
    }

    #[test]
    fn surfaces_azure_error_with_description() {
        let line = "GET /?error=access_denied&error_description=User+cancelled&state=s HTTP/1.1";
        match parse_redirect(line, "s").unwrap() {
            Redirect::Error(code, desc) => {
                assert_eq!(code, "access_denied");
                assert_eq!(desc, "User cancelled");
            }
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn missing_code_is_an_error() {
        assert!(parse_redirect("GET /?state=s HTTP/1.1", "s").is_err());
    }
}
