//! Shared fixtures: synthetic signals, channel definitions and a fake engine intake.

use std::sync::{Arc, Mutex};

use sutra_channels::{
    load_channel_definitions, AckDecision, BoxFuture, ChannelDefinition, InboundIntake,
    InboundMessage,
};
use tokio::sync::mpsc;

/// A sine tone.
pub fn tone(hz: f64, ms: u32, rate: u32, amplitude: f64) -> Vec<i16> {
    let n = (rate as u64 * ms as u64 / 1000) as usize;
    (0..n)
        .map(|i| {
            (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin()) as i16
        })
        .collect()
}

pub fn silence(ms: u32, rate: u32) -> Vec<i16> {
    vec![0; (rate as u64 * ms as u64 / 1000) as usize]
}

pub const CHANNELS: &str = r#"
channels:
  - name: start
    transport: voip
    bind: "number:+15551230000"
    silence-ms: 200
  - name: turn
    transport: voip
    event: call-turn
  - name: dtmf
    transport: voip
    event: call-dtmf
  - name: progress
    transport: voip
    event: call-progress
  - name: hangup
    transport: voip
    event: call-hangup
"#;

pub fn definitions(yaml: &str, module: &str) -> Vec<ChannelDefinition> {
    load_channel_definitions(yaml.as_bytes(), "acme", module, "1.0.0", "test")
        .expect("channels load")
}

/// Build the `llm` channels among `definitions` the way the `llm` transport does and publish
/// them to the process-wide channel registry the voip transport resolves `transcribe.channel` /
/// `analyze.channel` in.
pub fn publish_llm_channels(definitions: &[ChannelDefinition]) {
    let (map, failures) = sutra_transport_llm::parse_channels(definitions, |reference| {
        Ok(format!("resolved:{reference}"))
    });
    assert!(
        failures.is_empty(),
        "llm channels fail to load: {failures:?}"
    );
    let table = sutra_transport_llm::ChannelTable::from_map(map);
    sutra_transport_llm::publish_channels(&table, sutra_llm_spi::llm_channels());
}

/// Captures every delivered message; answers `decision`.
pub struct FakeIntake {
    pub decision: Mutex<AckDecision>,
    /// Decisions answered before `decision`, first first (a scripted requeue).
    pub script: Mutex<std::collections::VecDeque<AckDecision>>,
    pub messages: Mutex<Vec<InboundMessage>>,
    pub notify: mpsc::UnboundedSender<InboundMessage>,
}

impl FakeIntake {
    pub fn new() -> (Arc<FakeIntake>, mpsc::UnboundedReceiver<InboundMessage>) {
        let (notify, rx) = mpsc::unbounded_channel();
        (
            Arc::new(FakeIntake {
                decision: Mutex::new(AckDecision::Ack),
                script: Mutex::new(std::collections::VecDeque::new()),
                messages: Mutex::new(Vec::new()),
                notify,
            }),
            rx,
        )
    }
}

impl InboundIntake for FakeIntake {
    fn deliver(&self, message: InboundMessage) -> BoxFuture<'_, AckDecision> {
        Box::pin(async move {
            self.messages.lock().unwrap().push(message.clone());
            let _ = self.notify.send(message);
            let scripted = self.script.lock().unwrap().pop_front();
            scripted.unwrap_or(*self.decision.lock().unwrap())
        })
    }
}

/// Next delivered message on `channel`, skipping others, within 5 s.
pub async fn next_on(
    rx: &mut mpsc::UnboundedReceiver<InboundMessage>,
    channel: &str,
) -> InboundMessage {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let message = rx.recv().await.expect("intake open");
            if message.channel == channel {
                return message;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no '{channel}' message within 5 s"))
}

/// Serve `router` on an ephemeral port.
pub async fn serve(router: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

/// A minimal HTTP/1.1 POST; answers (status, body).
pub async fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

pub fn outbound(
    destination: &str,
    call_id: Option<&str>,
    body: &str,
    content_type: &str,
) -> sutra_channels::OutboundMessage {
    let mut headers = std::collections::BTreeMap::new();
    if let Some(id) = call_id {
        headers.insert("x-call-id".to_string(), id.to_string());
    }
    sutra_channels::OutboundMessage {
        destination: destination.to_string(),
        headers,
        body: body.as_bytes().to_vec(),
        content_type: Some(content_type.to_string()),
        outbox_key: format!("ob-{destination}-{}", call_id.unwrap_or("none")),
        traceparent: None,
    }
}
