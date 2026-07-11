use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::AuthError;
use crate::oauth::REDIRECT_PORT;

pub async fn wait_for_code(expected_state: &str) -> Result<String, AuthError> {
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .await
        .map_err(|e| AuthError::Server(format!("could not bind 127.0.0.1:{REDIRECT_PORT}: {e}")))?;
    accept_code(listener, expected_state).await
}

async fn accept_code(listener: TcpListener, expected_state: &str) -> Result<String, AuthError> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| AuthError::Server(e.to_string()))?;

        let mut buf = [0u8; 4096];
        let n = socket
            .read(&mut buf)
            .await
            .map_err(|e| AuthError::Server(e.to_string()))?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(target) = request_target(&request) else {
            respond(&mut socket, 400, "Bad Request").await;
            continue;
        };

        if !target.starts_with("/auth/callback") {
            respond(&mut socket, 404, "Not Found").await;
            continue;
        }

        let params = query_params(target);
        if let Some(error) = params.iter().find(|(k, _)| k == "error") {
            respond(
                &mut socket,
                400,
                "OpenAI did not approve the authorization request.",
            )
            .await;
            return Err(AuthError::Callback(error.1.clone()));
        }
        let state = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v);
        if state.map(String::as_str) != Some(expected_state) {
            respond(
                &mut socket,
                400,
                "State mismatch. Please try signing in again.",
            )
            .await;
            return Err(AuthError::Callback("state mismatch".to_owned()));
        }
        let Some(code) = params
            .into_iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v)
        else {
            respond(&mut socket, 400, "Missing authorization code.").await;
            return Err(AuthError::Callback("missing authorization code".to_owned()));
        };

        respond(
            &mut socket,
            200,
            "Your ChatGPT account is connected to tokio-agent.",
        )
        .await;
        return Ok(code);
    }
}

fn request_target(request: &str) -> Option<&str> {
    let first_line = request.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

fn query_params(target: &str) -> Vec<(String, String)> {
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let value = urlencoding::decode(v).ok()?.into_owned();
            Some((k.to_owned(), value))
        })
        .collect()
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = match status {
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let success = status == 200;
    let accent = if success { "#34d399" } else { "#fb7185" };
    let icon = if success { "✓" } else { "!" };
    let heading = if success {
        "You’re signed in"
    } else {
        "Sign-in didn’t complete"
    };
    let message = escape_html(message);
    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{heading} · tokio-agent</title>
<style>
:root {{ color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, sans-serif; }}
* {{ box-sizing: border-box; }}
body {{ min-height: 100vh; margin: 0; display: grid; place-items: center; color: #f4f4f5; background: radial-gradient(circle at 50% 20%, #20252b 0, #111418 42%, #090b0d 100%); }}
main {{ width: min(29rem, calc(100% - 2rem)); padding: 2.5rem; border: 1px solid #2d333a; border-radius: 1.25rem; background: rgba(20, 24, 28, .88); box-shadow: 0 24px 80px rgba(0, 0, 0, .38); text-align: center; }}
.brand {{ margin-bottom: 2rem; color: #a1a1aa; font: 600 .82rem ui-monospace, SFMono-Regular, Menlo, monospace; letter-spacing: .08em; }}
.icon {{ display: grid; width: 3rem; height: 3rem; margin: 0 auto 1.25rem; place-items: center; border: 1px solid {accent}; border-radius: 50%; color: {accent}; font-size: 1.35rem; box-shadow: 0 0 2rem color-mix(in srgb, {accent} 18%, transparent); }}
h1 {{ margin: 0 0 .75rem; font-size: 1.55rem; letter-spacing: -.025em; }}
p {{ margin: 0; color: #a1a1aa; line-height: 1.6; }}
.hint {{ margin-top: 1.75rem; padding-top: 1.5rem; border-top: 1px solid #292e34; color: #71717a; font-size: .85rem; }}
</style>
</head>
<body>
<main>
<div class="brand">tokio-agent</div>
<div class="icon">{icon}</div>
<h1>{heading}</h1>
<p>{message}</p>
<div class="hint">You can close this window and return to your terminal.</div>
</main>
</body>
</html>"#
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callback_returns_code_when_state_matches() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { accept_code(listener, "xyz").await });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /auth/callback?code=the-code&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert_eq!(server.await.unwrap().unwrap(), "the-code");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("You’re signed in"));
        assert!(response.contains("class=\"icon\">✓"));
    }

    #[tokio::test]
    async fn callback_rejects_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { accept_code(listener, "expected").await });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /auth/callback?code=c&state=wrong HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ = client.read_to_end(&mut response).await;

        assert!(server.await.unwrap().is_err());
    }
}
