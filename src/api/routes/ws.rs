//! WebSocket event streaming for the panel.

use crate::api::events::EventBus;
use crate::api::server::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use std::sync::Arc;

/// GET /ws/events — upgrades to WebSocket, streams PanelEvents as JSON.
///
/// Authentication: Browsers cannot set custom headers during the WebSocket
/// upgrade handshake, so the `/ws/` path is exempt from the auth middleware.
/// The client offers its token (static API token or JWT) as a
/// `Sec-WebSocket-Protocol` value — `new WebSocket(url, [token])` — and the
/// handshake echoes the accepted protocol back.  A `?auth=` query parameter
/// is deliberately NOT accepted: it leaks into access logs and browser
/// history (#653).
///
/// Enforces a hard cap of [`AppState::MAX_WS_CONNECTIONS`] concurrent
/// WebSocket connections via a semaphore stored in [`AppState`].  When the
/// cap is reached the handler responds with HTTP 503 before the upgrade so
/// the client gets a meaningful error rather than a silent hang.
pub async fn ws_events(
    mut ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    let Some(protocol) = select_auth_protocol(
        ws.requested_protocols(),
        &state.api_token,
        &state.jwt_secret,
    ) else {
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::UNAUTHORIZED)
            .body(axum::body::Body::from("Missing or invalid auth token"))
            .expect("response build is infallible");
    };
    ws.set_selected_protocol(protocol);

    // Try to acquire a connection slot.  `try_acquire_owned` is non-blocking:
    // it either succeeds immediately or returns `TryAcquireError::NoPermits`.
    let permit = match state.ws_semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return axum::response::Response::builder()
                .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                .body(axum::body::Body::from("Too many WebSocket connections"))
                .expect("response build is infallible")
        }
    };

    let event_bus = state.event_bus.clone();
    // Move the permit into the connection task so it is dropped (released)
    // only when the WebSocket connection closes.
    ws.on_upgrade(move |socket| handle_ws(socket, event_bus, permit))
}

/// Returns the offered `Sec-WebSocket-Protocol` value that authenticates
/// the client — the static API token or a valid JWT — so the handshake can
/// echo it back (#653).
fn select_auth_protocol<'a>(
    mut protocols: impl Iterator<Item = &'a axum::http::HeaderValue>,
    api_token: &str,
    jwt_secret: &str,
) -> Option<axum::http::HeaderValue> {
    protocols
        .find(|p| {
            p.to_str()
                .is_ok_and(|t| crate::api::auth::verify_token_or_jwt(t, api_token, jwt_secret))
        })
        .cloned()
}

async fn handle_ws(
    mut socket: WebSocket,
    event_bus: EventBus,
    // Held for the lifetime of the connection; dropped when this future
    // resolves, which releases the semaphore permit.
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let mut rx = event_bus.subscribe();
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(e) => {
                        let json = match serde_json::to_string(&e) {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {} // Ignore client messages for now
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // WebSocket handlers are hard to unit test directly.
    // Integration tests will cover the WS upgrade + event flow.
    // Here we test that the handler function exists and compiles.
    use super::*;
    use crate::api::server::AppState;

    #[test]
    fn test_ws_handler_compiles() {
        // Verify the handler signature is correct for axum routing.
        let _: fn(WebSocketUpgrade, State<Arc<AppState>>) -> _ = |ws, state| ws_events(ws, state);
    }

    #[test]
    fn test_select_auth_protocol() {
        let ok = axum::http::HeaderValue::from_static("s3cret-token");
        let bad = axum::http::HeaderValue::from_static("wrong");
        let junk = axum::http::HeaderValue::from_static("not.a.jwt");

        // First offered protocol that authenticates wins (offer order, not token type).
        let got = select_auth_protocol([&bad, &ok].into_iter(), "s3cret-token", "jwt-secret");
        assert_eq!(
            got.as_ref().and_then(|h| h.to_str().ok()),
            Some("s3cret-token")
        );

        // No offered protocol matches -> rejected.
        assert!(
            select_auth_protocol([&bad, &junk].into_iter(), "s3cret-token", "jwt-secret").is_none()
        );

        // Nothing offered at all -> rejected.
        assert!(select_auth_protocol(std::iter::empty(), "s3cret-token", "jwt-secret").is_none());
    }

    #[test]
    fn test_ws_semaphore_exhaustion_reduces_permits() {
        // Verify that acquiring all permits leaves the semaphore at zero.
        let sem = Arc::new(tokio::sync::Semaphore::new(AppState::MAX_WS_CONNECTIONS));
        let mut permits = Vec::new();
        for _ in 0..AppState::MAX_WS_CONNECTIONS {
            permits.push(sem.clone().try_acquire_owned().expect("permit available"));
        }
        assert_eq!(sem.available_permits(), 0);
        // The next acquire should fail.
        assert!(sem.clone().try_acquire_owned().is_err());
        // Releasing one permit makes room again.
        drop(permits.pop());
        assert_eq!(sem.available_permits(), 1);
    }
}
