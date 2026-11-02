//! Bounded, memory-only signaling. Native hosts prove possession of their stable identity.
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use lantern_protocol::{Envelope, VERSION};
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

#[derive(Clone)]
struct Route {
    owner: String,
    sender: mpsc::Sender<Message>,
}
#[derive(Default)]
struct Registry {
    routes: HashMap<String, Route>,
    per_ip: HashMap<IpAddr, usize>,
}
#[derive(Clone)]
struct AppState {
    registry: Arc<Mutex<Registry>>,
    origin: String,
}

pub fn router(origin: String, web_root: &str) -> Router {
    Router::new()
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok", "protocol":VERSION})) }),
        )
        .route("/ws", get(upgrade))
        .fallback_service(
            ServeDir::new(web_root)
                .not_found_service(ServeFile::new(format!("{web_root}/index.html"))),
        )
        .with_state(AppState {
            registry: Arc::default(),
            origin,
        })
}

async fn upgrade(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Native clients omit Origin. A browser must come from our exact deployment origin.
    if let Some(origin) = headers.get("origin") {
        if origin.to_str().ok() != Some(state.origin.as_str()) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    {
        let mut registry = state.registry.lock().unwrap();
        if registry.routes.len() >= 512
            || registry.per_ip.get(&addr.ip()).copied().unwrap_or(0) >= 32
        {
            return StatusCode::TOO_MANY_REQUESTS.into_response();
        }
        *registry.per_ip.entry(addr.ip()).or_default() += 1;
    }
    ws.max_message_size(128 * 1024)
        .max_frame_size(128 * 1024)
        .on_upgrade(move |socket| connection(socket, state, addr.ip()))
        .into_response()
}

fn wire(value: Value) -> Message {
    Message::Text(value.to_string().into())
}
fn protocol_error(code: &str) -> Message {
    wire(json!({"type":"error", "code":code, "message":code}))
}

fn registered_id(
    key: &str,
    signature: &str,
    ephemeral: &str,
    nonce: &str,
) -> anyhow::Result<String> {
    let raw = B64.decode(key)?;
    let verifying = VerifyingKey::from_sec1_bytes(&raw)?;
    let sig = Signature::from_slice(&B64.decode(signature)?)?;
    verifying.verify(
        &serde_json::to_vec(&["lantern-register-v1", ephemeral, nonce])?,
        &sig,
    )?;
    Ok(hex::encode(Sha256::digest(raw)))
}

async fn connection(socket: WebSocket, state: AppState, ip: IpAddr) {
    let id = Uuid::new_v4().to_string();
    let mut random = [0; 32];
    OsRng.fill_bytes(&mut random);
    let nonce = B64.encode(random);
    let (tx, mut rx) = mpsc::channel::<Message>(64);
    state.registry.lock().unwrap().routes.insert(
        id.clone(),
        Route {
            owner: id.clone(),
            sender: tx.clone(),
        },
    );
    let (mut sink, mut source) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if tokio::time::timeout(Duration::from_secs(10), sink.send(message))
                .await
                .map_or(true, |result| result.is_err())
            {
                break;
            }
        }
    });
    let _ = tx.try_send(wire(json!({"type":"registered", "id":id, "nonce":nonce})));
    let mut stable: Option<String> = None;
    let mut allowance = 120usize;
    let mut window = Instant::now();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut last_activity = Instant::now();
    loop {
        let message = tokio::select! {
            message = source.next() => match message { Some(Ok(m)) => m, _ => break },
            _ = ping.tick() => {
                if last_activity.elapsed() > Duration::from_secs(60) { break; }
                if tx.try_send(Message::Ping(Vec::new().into())).is_err() { break; }
                continue;
            }
        };
        last_activity = Instant::now();
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                break;
            }
            continue;
        };
        if window.elapsed() >= Duration::from_secs(1) {
            allowance = 120;
            window = Instant::now();
        }
        if allowance == 0 {
            let _ = tx.try_send(protocol_error("rate_limited"));
            break;
        }
        allowance -= 1;
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                let _ = tx.try_send(protocol_error("invalid_json"));
                continue;
            }
        };
        if value["type"] == "register_host" {
            if stable.is_some() {
                let _ = tx.try_send(protocol_error("already_registered"));
                continue;
            }
            let result = registered_id(
                value["key"].as_str().unwrap_or(""),
                value["signature"].as_str().unwrap_or(""),
                &id,
                &nonce,
            );
            match result {
                Ok(alias) => {
                    let inserted = {
                        let mut registry = state.registry.lock().unwrap();
                        if registry.routes.contains_key(&alias) {
                            false
                        } else {
                            registry.routes.insert(
                                alias.clone(),
                                Route {
                                    owner: id.clone(),
                                    sender: tx.clone(),
                                },
                            );
                            true
                        }
                    };
                    if inserted {
                        stable = Some(alias.clone());
                        let _ = tx.try_send(wire(json!({"type":"host_registered", "id":alias})));
                    } else {
                        let _ = tx.try_send(protocol_error("host_already_online"));
                    }
                }
                Err(_) => {
                    let _ = tx.try_send(protocol_error("invalid_identity"));
                }
            }
            continue;
        }
        let Ok(mut envelope) = serde_json::from_value::<Envelope>(value) else {
            let _ = tx.try_send(protocol_error("invalid_envelope"));
            continue;
        };
        if envelope.version != VERSION {
            let _ = tx.try_send(protocol_error("incompatible_version"));
            continue;
        }
        if envelope.to.len() > 100 {
            let _ = tx.try_send(protocol_error("invalid_destination"));
            continue;
        }
        // Never trust the sender supplied by the socket's caller.
        envelope.from = stable.clone().unwrap_or_else(|| id.clone());
        let route = state
            .registry
            .lock()
            .unwrap()
            .routes
            .get(&envelope.to)
            .cloned();
        let error = if let Some(route) = route {
            if route
                .sender
                .try_send(wire(serde_json::to_value(&envelope).unwrap()))
                .is_ok()
            {
                None
            } else {
                Some("peer_busy")
            }
        } else {
            Some("offline")
        };
        if let Some(code) = error {
            let _ = tx.try_send(wire(json!({"version":VERSION,"to":envelope.from,"from":envelope.to,"payload":{"type":"error","code":code,"message":code}})));
        }
    }
    {
        let mut registry = state.registry.lock().unwrap();
        registry.routes.retain(|_, route| route.owner != id);
        if let Some(count) = registry.per_ip.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                registry.per_ip.remove(&ip);
            }
        }
    }
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{client::IntoClientRequest, Message as ClientMessage},
    };
    async fn start() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router("https://remote.example".into(), "/does-not-exist");
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (format!("ws://{addr}/ws"), task)
    }
    #[tokio::test]
    async fn relay_rewrites_spoofed_sender_and_rejects_foreign_origin() {
        let (url, task) = start().await;
        let mut bad = url.clone().into_client_request().unwrap();
        bad.headers_mut()
            .insert("origin", "https://evil.example".parse().unwrap());
        assert!(connect_async(bad).await.is_err());
        let (mut a, _) = connect_async(&url).await.unwrap();
        let (mut b, _) = connect_async(&url).await.unwrap();
        async fn registration<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> String
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        {
            loop {
                let m = ws.next().await.unwrap().unwrap();
                if let ClientMessage::Text(t) = m {
                    return serde_json::from_str::<Value>(&t).unwrap()["id"]
                        .as_str()
                        .unwrap()
                        .into();
                }
            }
        }
        let aid = registration(&mut a).await;
        let bid = registration(&mut b).await;
        a.send(ClientMessage::Text(
            json!({"version":1,"to":bid,"from":"spoofed","payload":{"type":"probe"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        loop {
            if let ClientMessage::Text(t) = b.next().await.unwrap().unwrap() {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["from"], aid);
                assert_eq!(v["payload"]["type"], "probe");
                break;
            }
        }
        task.abort();
    }
    #[test]
    fn stable_registration_is_bound_to_server_nonce_and_socket() {
        let key = SigningKey::random(&mut OsRng);
        let raw = key.verifying_key().to_encoded_point(false);
        let sig: Signature =
            key.sign(&serde_json::to_vec(&["lantern-register-v1", "socket-a", "nonce-a"]).unwrap());
        let public = B64.encode(raw.as_bytes());
        let signature = B64.encode(sig.to_bytes());
        assert!(registered_id(&public, &signature, "socket-a", "nonce-a").is_ok());
        assert!(registered_id(&public, &signature, "socket-b", "nonce-a").is_err());
        assert!(registered_id(&public, &signature, "socket-a", "nonce-b").is_err());
    }
}
