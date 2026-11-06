//! Live probe: places Microsoft's Test Call (the echo bot, `UserInitiatedTestCall`,
//! which rings no one) with an `rv-voice` sidecar's offer and media, and prints
//! what Teams answers and what the media does.
//!
//!   secret-tool lookup application me.barrut.RocketVibe kind session account '<key>' \
//!     | RV_VOICE=path/to/rv-voice cargo run --example teams-echo
//!
//! The account's secret comes on stdin and is never printed or written; neither is
//! any token.

use std::io::Read;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rv_teams::auth::{Tokens, from_secret};
use rv_teams::calling::{self, CLIENT, CallEvent, Caller, EPCONV};
use rv_teams::frames::{self, Frame};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).unwrap();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn shape(v: &Value, depth: usize) -> String {
    match v {
        Value::Object(o) if depth > 0 => {
            let parts: Vec<String> = o.iter().map(|(k, c)| format!("{k}: {}", shape(c, depth - 1))).collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Object(o) => format!("{{{} keys}}", o.len()),
        Value::Array(a) => format!("[{}]", a.len()),
        Value::String(s) => format!("str({})", s.len()),
        other => other.to_string(),
    }
}

#[tokio::main]
async fn main() {
    let mut secret = String::new();
    std::io::stdin().read_to_string(&mut secret).unwrap();
    let session = from_secret(&serde_json::from_str(&secret).expect("secret json")).expect("a Teams account");
    secret.clear();
    let me = session.user_id.clone();
    let tokens = Tokens::new(&session);
    rv_provider::tls::ensure_provider();

    let (skype, routes) = tokens.chat().await.expect("chat tokens");
    let bearer = tokens.calling().await.expect("calling token");
    println!("tokens: skype and ic3 minted");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();

    let epid = uuid();
    let info = json!({"cv": "2026.10.0.1", "ua": "TeamsCDL", "hr": "", "v": "1415/26.10.0.0"}).to_string();
    let mut allocate = url::Url::parse(&routes.trouter).unwrap().join("/v4/a").unwrap();
    allocate.query_pairs_mut().append_pair("epid", &epid).append_pair("tc", &info);
    let answer: Value = http
        .post(allocate)
        .header("X-Skypetoken", skype.as_str())
        .header("content-type", "text/plain")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let socketio = url::Url::parse(answer["socketio"].as_str().unwrap()).unwrap();
    let surl = answer["surl"].as_str().unwrap().to_owned();
    let params = answer["connectparams"].clone();
    let mut query: Vec<(String, String)> =
        params.as_object().unwrap().iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect();
    query.extend([
        ("v".to_owned(), "v4".to_owned()),
        ("tc".to_owned(), info.clone()),
        ("timeout".to_owned(), "40".to_owned()),
        ("auth".to_owned(), "true".to_owned()),
        ("epid".to_owned(), epid.clone()),
        ("cor_id".to_owned(), uuid()),
        ("con_num".to_owned(), "1_1".to_owned()),
    ]);
    let mut handshake = socketio.join("socket.io/1/").unwrap();
    handshake.query_pairs_mut().extend_pairs(&query);
    let text = http.get(handshake).header("X-Skypetoken", skype.as_str()).send().await.unwrap().text().await.unwrap();
    let sid = text.split(':').next().unwrap().to_owned();
    let mut socket_url = socketio.join(&format!("socket.io/1/websocket/{sid}")).unwrap();
    socket_url.set_scheme("wss").unwrap();
    socket_url.query_pairs_mut().extend_pairs(&query);
    let mut request = socket_url.as_str().into_client_request().unwrap();
    request.headers_mut().insert("X-Skypetoken", skype.as_str().parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (mut sink, mut stream) = socket.split();
    while let Some(Ok(Message::Text(text))) = stream.next().await {
        if frames::parse(text.as_str()) == Frame::Connect {
            sink.send(Message::text(frames::authenticate(&skype, &params))).await.unwrap();
            break;
        }
    }
    let registration = json!({
        "clientDescription": {"appId": "SkypeSpacesWeb", "aesKey": "", "languageId": "en-US", "platform": "edge",
            "templateKey": "SkypeSpacesWeb_2.6", "platformUIVersion": "49/26.10.0.0"},
        "registrationId": uuid(), "nodeId": "",
        "transports": {"TROUTER": [{"context": "", "path": surl, "ttl": 3600}]},
    });
    let registered = http
        .post(&routes.registrar)
        .header("X-Skypetoken", skype.as_str())
        .json(&registration)
        .send()
        .await
        .unwrap()
        .status();
    println!("trouter connected, calling registration: {registered}");

    let endpoint = uuid();
    let participant = uuid();
    let random = uuid();
    let caller = Caller {
        mri: &me,
        name: &session.username,
        endpoint: &endpoint,
        participant: &participant,
        surl: &surl,
        random: &random,
        client: CLIENT,
    };

    let mut child = tokio::process::Command::new(std::env::var("RV_VOICE").expect("set RV_VOICE"))
        .env("RV_VOICE_FAKE_AUDIO", "sine")
        .env("RV_CALL_DEBUG", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut to_sidecar = child.stdin.take().unwrap();
    let mut from_sidecar = BufReader::new(child.stdout.take().unwrap()).lines();
    let command = |v: Value| format!("{v}\n");
    to_sidecar.write_all(command(json!({"type": "connect_external", "identity": endpoint})).as_bytes()).await.unwrap();
    let offer = loop {
        let line = from_sidecar.next_line().await.unwrap().expect("the sidecar went away");
        let event: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        if event["type"] == "local_offer" {
            break event["sdp"].as_str().unwrap().to_owned();
        }
    };
    println!("sidecar offered {} bytes", offer.len());
    if let Ok(path) = std::env::var("TEAMS_PROBE_OFFER") {
        std::fs::write(path, calling::teams_offer(&offer)).unwrap();
    }

    let chain = uuid();
    let post = |url: String, body: Value| {
        http.post(url)
            .bearer_auth(bearer.as_str())
            .header("x-microsoft-skype-chain-id", chain.clone())
            .header("x-microsoft-skype-message-id", uuid())
            .header("x-microsoft-skype-client", CLIENT)
            .header("x-ms-migration", "True")
            .header("origin", "https://teams.cloud.microsoft")
            .header("referer", "https://teams.cloud.microsoft/")
            .json(&body)
            .send()
    };
    let payload = calling::place_call(&caller, &[], None, &calling::teams_offer(&offer), &uuid());
    let response = post(EPCONV.to_owned(), payload).await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    println!("epconv: {status}");
    if !status.is_success() {
        println!("{}", body.chars().take(800).collect::<String>());
        return;
    }
    let created: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let links = created.get("links").cloned().unwrap_or(Value::Null);

    let seconds = std::env::var("TEAMS_PROBE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(45);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let mut last_participants = String::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            line = from_sidecar.next_line() => {
                let Ok(Some(line)) = line else { println!("sidecar gone"); break };
                let event: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                match event["type"].as_str() {
                    Some("state") | Some("disconnected") | Some("error") => println!("sidecar {line}"),
                    Some("participants") => {
                        let shown = event["participants"].to_string();
                        if shown != last_participants {
                            last_participants = shown;
                        }
                    }
                    _ => {}
                }
            }
            frame = stream.next() => {
                let Some(Ok(Message::Text(text))) = frame else { println!("trouter gone"); break };
                match frames::parse(text.as_str()) {
                    Frame::Heartbeat => {
                        let _ = sink.send(Message::text(frames::HEARTBEAT)).await;
                    }
                    Frame::Request { envelope, ack } => {
                        if let Some(reply) = frames::reply(&envelope) {
                            let _ = sink.send(Message::text(reply)).await;
                        }
                        if let Some(id) = ack {
                            let _ = sink.send(Message::text(frames::ack(&id))).await;
                        }
                        let url = envelope.get("url").and_then(Value::as_str).unwrap_or("");
                        let tail = url.split("callAgent/").nth(1).and_then(|t| t.splitn(3, '/').nth(2)).unwrap_or("(other)");
                        let event = calling::call_event(&envelope);
                        println!("push {tail}: {}", match &event {
                            CallEvent::Accepted { sdp, .. } => format!("accepted, {} bytes of SDP", sdp.len()),
                            CallEvent::Ended { phrase } => format!("ended {phrase:?}"),
                            CallEvent::Roster(changes) => format!("roster {changes:?}"),
                            CallEvent::Other => shape(&calling::pushed_body(&envelope), 1),
                        });
                        match event {
                            CallEvent::Accepted { sdp, acknowledge, .. } => {
                                let Some(answer) = calling::webrtc_answer(&sdp) else {
                                    println!("no audio in the answer");
                                    break;
                                };
                                to_sidecar
                                    .write_all(command(json!({"type": "remote_answer", "sdp": answer})).as_bytes())
                                    .await
                                    .unwrap();
                                if let Some(link) = acknowledge {
                                    let acked = post(link, calling::acknowledge_acceptance(&caller)).await;
                                    println!("acknowledged: {:?}", acked.map(|r| r.status().to_string()));
                                }
                                if let Some(add) = links.get("addParticipant").and_then(Value::as_str) {
                                    let invited = post(add.to_owned(), calling::invite_test_bot(&caller, &uuid())).await;
                                    println!("invited the Test Call bot: {:?}", invited.map(|r| r.status().to_string()));
                                }
                            }
                            CallEvent::Ended { .. } => break,
                            CallEvent::Roster(_) | CallEvent::Other => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = to_sidecar.write_all(command(json!({"type": "disconnect"})).as_bytes()).await;
    if let Some(leave) = links.get("leave").and_then(Value::as_str) {
        let left = post(leave.to_owned(), calling::leave_call(&caller)).await.map(|r| r.status().to_string());
        println!("leave: {left:?}");
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
