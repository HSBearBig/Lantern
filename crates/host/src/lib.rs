//! Native host actor. Pairing and trust live at the endpoints, never at the relay.
pub mod identity;
mod rtc;
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use identity::{random_nonce, sign, verify, IdentityStore, PairedBrowser, Pairing, Store};
use lantern_platform::CaptureOptions;
use lantern_protocol::{Envelope, VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

#[derive(Clone)]
pub struct HostConfig {
    pub server: Url,
    pub name: String,
    pub data_dir: PathBuf,
    pub options: CaptureOptions,
    pub pair_on_start: bool,
    pub allow_insecure_localhost: bool,
}
impl HostConfig {
    pub fn validate(&self) -> Result<()> {
        if self.server.scheme() != "https" {
            let local = matches!(
                self.server.host_str(),
                Some("localhost" | "127.0.0.1" | "::1")
            );
            if !(self.allow_insecure_localhost && self.server.scheme() == "http" && local) {
                bail!("Server must use HTTPS; HTTP is permitted only for explicit localhost development");
            }
        }
        if !self.server.username().is_empty()
            || self.server.password().is_some()
            || self.server.query().is_some()
            || self.server.fragment().is_some()
        {
            bail!("Server URL must not contain credentials, query, or fragment");
        }
        if self.name.is_empty() || self.name.len() > 100 {
            bail!("Device name must contain 1–100 bytes");
        }
        if !(1..=60).contains(&self.options.fps)
            || self.options.max_width < 2
            || self.options.max_height < 2
        {
            bail!("Invalid capture settings");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub host_id: String,
    pub name: String,
    pub online: bool,
    pub enabled: bool,
    pub session: Option<String>,
    pub pairing_url: Option<String>,
    pub pairing_expires_in: u64,
    pub paired_browsers: Vec<BrowserStatus>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserStatus {
    pub id: String,
    pub name: String,
}
impl HostStatus {
    pub fn starting(name: String) -> Self {
        Self {
            host_id: String::new(),
            name,
            online: false,
            enabled: true,
            session: None,
            pairing_url: None,
            pairing_expires_in: 0,
            paired_browsers: vec![],
            error: None,
        }
    }
}
#[derive(Debug)]
pub enum HostCommand {
    Pair,
    Disconnect,
    SetEnabled(bool),
    Revoke(String),
    Shutdown,
}
struct Challenge {
    client: String,
    nonce: String,
    created: Instant,
}
struct Host {
    config: HostConfig,
    store: Store,
    pairing: Option<(Pairing, Instant)>,
    pending: HashMap<String, Challenge>,
    session: Option<(String, rtc::Session)>,
    status: HostStatus,
    updates: watch::Sender<HostStatus>,
}
impl Host {
    fn publish(&mut self) {
        self.status.paired_browsers = self
            .store
            .pairs
            .iter()
            .map(|(id, p)| BrowserStatus {
                id: id.clone(),
                name: p.name.clone(),
            })
            .collect();
        if let Some((_, created)) = self.pairing.as_ref() {
            self.status.pairing_expires_in = 300u64.saturating_sub(created.elapsed().as_secs());
            if self.status.pairing_expires_in == 0 {
                self.pairing = None;
                self.status.pairing_url = None;
            }
        }
        self.updates.send_replace(self.status.clone());
    }
    fn pair(&mut self) {
        let pair = Pairing::new();
        let fragment = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("host", &self.status.host_id)
            .append_pair("secret", &pair.secret_base64())
            .append_pair("key", &self.store.host_key())
            .append_pair("name", &self.config.name)
            .finish();
        let mut url = self.config.server.clone();
        url.set_fragment(Some(&fragment));
        self.pairing = Some((pair, Instant::now()));
        self.status.pairing_url = Some(url.to_string());
        self.publish();
    }
    async fn disconnect(&mut self) {
        if let Some((_, session)) = self.session.take() {
            session.close().await;
        }
        self.status.session = None;
        self.pending.clear();
        self.publish();
    }
    async fn command(&mut self, command: HostCommand) -> Result<bool> {
        match command {
            HostCommand::Pair => {
                if self.status.enabled {
                    self.pair();
                }
            }
            HostCommand::Disconnect => self.disconnect().await,
            HostCommand::SetEnabled(enabled) => {
                self.status.enabled = enabled;
                if !enabled {
                    self.pairing = None;
                    self.status.pairing_url = None;
                    self.disconnect().await;
                }
            }
            HostCommand::Revoke(id) => {
                self.store.reload_pairs()?;
                self.store.pairs.remove(&id);
                self.store.persist_pairs()?;
                if self
                    .session
                    .as_ref()
                    .is_some_and(|(client, _)| client == &id)
                {
                    self.disconnect().await;
                }
                self.pending.retain(|_, challenge| challenge.client != id);
            }
            HostCommand::Shutdown => {
                self.disconnect().await;
                return Ok(true);
            }
        }
        self.publish();
        Ok(false)
    }
    async fn tick(&mut self) {
        if let Err(error) = self.store.reload_pairs() {
            self.status.error = Some(format!("Cannot reload pairing trust: {error}"));
            self.disconnect().await;
        } else if self
            .session
            .as_ref()
            .is_some_and(|(client, _)| !self.store.pairs.contains_key(client))
        {
            self.disconnect().await;
        }
        self.pending
            .retain(|_, c| c.created.elapsed() < Duration::from_secs(30));
        if self.session.as_ref().is_some_and(|(_, s)| s.finished()) {
            self.disconnect().await;
        }
        self.publish();
    }
    async fn payload(&mut self, from: &str, payload: &Value) -> Result<Value> {
        if !self.status.enabled {
            bail!("disabled");
        }
        self.store.reload_pairs()?;
        match payload["type"].as_str().unwrap_or("") {
            "pair" => {
                let client = field(payload, "clientId")?;
                let key = field(payload, "clientKey")?;
                let nonce = field(payload, "nonce")?;
                let proof = field(payload, "proof")?;
                let name = field(payload, "name")?;
                if name.is_empty() || name.len() > 100 {
                    bail!("invalid_browser_name");
                }
                let (pair, _) = self.pairing.as_mut().context("pairing_closed")?;
                pair.accept(&self.status.host_id, client, key, nonce, proof)?;
                let previous = self.store.pairs.insert(
                    client.to_owned(),
                    PairedBrowser {
                        name: name.into(),
                        key: key.into(),
                    },
                );
                if let Err(error) = self.store.persist_pairs() {
                    match previous {
                        Some(p) => {
                            self.store.pairs.insert(client.into(), p);
                        }
                        None => {
                            self.store.pairs.remove(client);
                        }
                    }
                    return Err(error);
                }
                let host_key = self.store.host_key();
                let ack = pair.acknowledgement(&self.status.host_id, &host_key, client, nonce);
                pair.consume();
                self.pairing = None;
                self.status.pairing_url = None;
                self.status.pairing_expires_in = 0;
                self.publish();
                Ok(
                    json!({"type":"pair_ok","hostId":self.status.host_id,"hostKey":host_key,"nonce":nonce,"proof":ack}),
                )
            }
            "connect" => {
                if self.session.is_some() {
                    bail!("busy");
                }
                let client = field(payload, "clientId")?;
                if !self.store.pairs.contains_key(client) {
                    bail!("not_paired");
                }
                if self.pending.len() >= 32 && !self.pending.contains_key(from) {
                    bail!("busy");
                }
                let nonce = random_nonce();
                self.pending.insert(
                    from.into(),
                    Challenge {
                        client: client.into(),
                        nonce: nonce.clone(),
                        created: Instant::now(),
                    },
                );
                Ok(json!({"type":"challenge","nonce":nonce}))
            }
            "offer" => {
                if self.session.is_some() {
                    bail!("busy");
                }
                let challenge = self
                    .pending
                    .remove(from)
                    .context("challenge_missing_or_replayed")?;
                let client = field(payload, "clientId")?;
                let nonce = field(payload, "nonce")?;
                if challenge.client != client
                    || challenge.nonce != nonce
                    || challenge.created.elapsed() >= Duration::from_secs(30)
                {
                    bail!("challenge_expired_or_mismatched");
                }
                let browser = self.store.pairs.get(client).context("not_paired")?;
                let sdp = field(payload, "sdp")?;
                if sdp.len() > 100_000 || !sdp.contains("a=fingerprint:sha-256 ") {
                    bail!("invalid_sdp");
                }
                verify(
                    &browser.key,
                    field(payload, "signature")?,
                    &["lantern-offer-v1", &self.status.host_id, client, nonce, sdp],
                )?;
                let (session, answer, display) =
                    rtc::answer(sdp.into(), self.config.options).await?;
                let signature = sign(
                    self.store.signing_key(),
                    &[
                        "lantern-answer-v1",
                        &self.status.host_id,
                        client,
                        nonce,
                        sdp,
                        &answer,
                    ],
                );
                self.status.session = Some(browser.name.clone());
                self.session = Some((client.into(), session));
                self.publish();
                Ok(
                    json!({"type":"answer","nonce":nonce,"sdp":answer,"signature":signature,"display":display}),
                )
            }
            _ => bail!("unsupported_message"),
        }
    }
}
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing_{key}"))
}

/// Runs until Shutdown or all command senders are dropped. Existing media survives relay outages.
pub async fn run(
    config: HostConfig,
    mut commands: mpsc::Receiver<HostCommand>,
    updates: watch::Sender<HostStatus>,
) -> Result<()> {
    config.validate()?;
    let store = Store::open(config.data_dir.clone())?;
    let mut status = HostStatus::starting(config.name.clone());
    status.host_id = store.host_id();
    let initial_pair = config.pair_on_start || store.pairs.is_empty();
    let mut host = Host {
        config,
        store,
        pairing: None,
        pending: HashMap::new(),
        session: None,
        status,
        updates,
    };
    host.publish();
    if initial_pair {
        host.pair();
    }
    let mut url = host.config.server.join("/ws")?;
    url.set_scheme(if host.config.server.scheme() == "https" {
        "wss"
    } else {
        "ws"
    })
    .map_err(|_| anyhow::anyhow!("Invalid server scheme"))?;
    let mut retry = 1u64;
    let mut offline_tick = tokio::time::interval(Duration::from_secs(1));
    'reconnect: loop {
        let connection = tokio::select! {
            connection = tokio::time::timeout(Duration::from_secs(10),tokio_tungstenite::connect_async(url.as_str())) => connection,
            command = commands.recv() => { if match command {Some(c) => host.command(c).await?,None => true} {break;} continue; }
        };
        let mut ws = match connection {
            Ok(Ok((ws, _))) => {
                retry = 1;
                ws
            }
            _ => {
                host.status.online = false;
                host.status.error =
                    Some("連線協調服務暫時無法連線；既有直連工作階段不受影響".into());
                host.publish();
                let delay = tokio::time::sleep(Duration::from_secs(retry));
                tokio::pin!(delay);
                loop {
                    tokio::select! {
                        _ = &mut delay => break,
                    _ = offline_tick.tick() => host.tick().await,
                        command = commands.recv() => { if match command { Some(c) => host.command(c).await?, None => true } { break 'reconnect; } }
                    }
                }
                retry = (retry * 2).min(30);
                continue;
            }
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                command = commands.recv() => { if match command {Some(c) => host.command(c).await?, None => true} { break 'reconnect; } },
                _ = tick.tick() => host.tick().await,
                incoming = ws.next() => {
                    let message = match incoming {Some(Ok(m)) => m,_ => break};
                    match message {
                        Message::Text(text) => {
                            let value:Value = match serde_json::from_str(&text) {Ok(v)=>v,Err(_)=>continue};
                            if value["type"] == "registered" {
                                let ephemeral = field(&value,"id")?; let nonce = field(&value,"nonce")?;
                                let response = json!({"type":"register_host","key":host.store.host_key(),"signature":sign(host.store.signing_key(), &["lantern-register-v1",ephemeral,nonce])});
                                if ws.send(Message::Text(response.to_string().into())).await.is_err() { break; }
                            } else if value["type"] == "host_registered" {
                                if value["id"] != host.status.host_id { bail!("Relay returned incorrect host identity"); }
                                host.status.online = true; host.status.error = None; host.publish();
                            } else if value["type"] == "error" {
                                host.status.error = Some(field(&value,"code").unwrap_or("relay_error").into());host.publish();
                                if value["code"] == "host_already_online" { bail!("Another host instance is already running"); }
                            } else if let Ok(envelope) = serde_json::from_value::<Envelope>(value) {
                                if envelope.version != VERSION || envelope.to != host.status.host_id { continue; }
                                let response = match host.payload(&envelope.from,&envelope.payload).await {
                                    Ok(p)=>p,
                                    Err(e)=>{ let reason = e.to_string(); let code = match reason.as_str() {"busy"|"not_paired"|"disabled"|"pairing_closed"=>reason.as_str(),_=>"request_rejected"}; json!({"type":"error","code":code,"message":reason}) }
                                };
                                let response = Envelope {version:VERSION,to:envelope.from,from:String::new(),payload:response};
                                if ws.send(Message::Text(serde_json::to_string(&response)?.into())).await.is_err() { break; }
                            }
                        }
                        Message::Ping(data) => { if ws.send(Message::Pong(data)).await.is_err() {break;} }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            }
        }
        host.status.online = false;
        host.pending.clear();
        host.publish();
    }
    host.status.online = false;
    host.status.enabled = false;
    host.pairing = None;
    host.status.pairing_url = None;
    host.status.pairing_expires_in = 0;
    host.disconnect().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> HostConfig {
        HostConfig {
            server: Url::parse("https://remote.example").unwrap(),
            name: "Test Mac".into(),
            data_dir: PathBuf::new(),
            options: CaptureOptions::default(),
            pair_on_start: false,
            allow_insecure_localhost: false,
        }
    }
    #[test]
    fn refuses_insecure_lan_and_credentialed_urls() {
        let mut c = config();
        assert!(c.validate().is_ok());
        c.server = Url::parse("http://192.168.1.20:8787").unwrap();
        c.allow_insecure_localhost = true;
        assert!(c.validate().is_err());
        c.server = Url::parse("http://localhost:8787").unwrap();
        assert!(c.validate().is_ok());
        c.server = Url::parse("https://user:pass@remote.example").unwrap();
        assert!(c.validate().is_err());
    }
    #[tokio::test]
    async fn pairing_persists_then_revocation_rejects_reconnect() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::temporary(dir.path().join("pairs.json"));
        let mut status = HostStatus::starting("Test".into());
        status.host_id = store.host_id();
        let (tx, _) = watch::channel(status.clone());
        let mut h = Host {
            config: config(),
            store,
            pairing: None,
            pending: HashMap::new(),
            session: None,
            status,
            updates: tx,
        };
        h.pair();
        let invite = Url::parse(h.status.pairing_url.as_ref().unwrap()).unwrap();
        let values: HashMap<_, _> =
            url::form_urlencoded::parse(invite.fragment().unwrap().as_bytes())
                .into_owned()
                .collect();
        let secret = B64.decode(&values["secret"]).unwrap();
        let key = p256::ecdsa::SigningKey::random(&mut rand::rngs::OsRng);
        let public = identity::public_key(&key);
        let client = identity::device_id(&public).unwrap();
        let nonce = random_nonce();
        let proof = identity::mac(
            &secret,
            &[
                "lantern-pair-v1",
                &h.status.host_id,
                &client,
                &public,
                &nonce,
            ],
        );
        let request = json!({"type":"pair","clientId":client,"clientKey":public,"name":"Chrome","nonce":nonce,"proof":proof});
        let ack = h.payload("browser-socket", &request).await.unwrap();
        assert_eq!(ack["nonce"], nonce);
        identity::verify_mac(
            &secret,
            ack["proof"].as_str().unwrap(),
            &[
                "lantern-pair-ok-v1",
                &h.status.host_id,
                &h.store.host_key(),
                &client,
                &nonce,
            ],
        )
        .unwrap();
        assert!(h.pairing.is_none());
        assert!(h.status.pairing_url.is_none());
        assert!(h.payload("browser-socket", &request).await.is_err());
        h.store.pairs.clear();
        h.store.reload_pairs().unwrap();
        assert!(h.store.pairs.contains_key(&client));
        let challenge = h
            .payload(
                "browser-socket",
                &json!({"type":"connect","clientId":client}),
            )
            .await
            .unwrap();
        assert_eq!(challenge["type"], "challenge");
        h.command(HostCommand::Revoke(client.clone()))
            .await
            .unwrap();
        assert!(h.pending.is_empty());
        assert!(h
            .payload(
                "browser-socket",
                &json!({"type":"connect","clientId":client})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("not_paired"));
    }
    #[tokio::test]
    async fn expired_or_replayed_challenge_never_opens_native_capture() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::temporary(dir.path().join("pairs.json"));
        let status = HostStatus::starting("Test".into());
        let (tx, _) = watch::channel(status.clone());
        let mut h = Host {
            config: config(),
            store,
            pairing: None,
            pending: HashMap::new(),
            session: None,
            status,
            updates: tx,
        };
        let offer = json!({"type":"offer","clientId":"client","nonce":"nonce","sdp":"invalid","signature":"invalid"});
        assert!(h
            .payload("socket", &offer)
            .await
            .unwrap_err()
            .to_string()
            .contains("challenge_missing"));
        h.pending.insert(
            "socket".into(),
            Challenge {
                client: "client".into(),
                nonce: "nonce".into(),
                created: Instant::now() - Duration::from_secs(31),
            },
        );
        assert!(h
            .payload("socket", &offer)
            .await
            .unwrap_err()
            .to_string()
            .contains("expired"));
        assert!(!h.pending.contains_key("socket"));
        assert!(h.session.is_none());
    }
}
