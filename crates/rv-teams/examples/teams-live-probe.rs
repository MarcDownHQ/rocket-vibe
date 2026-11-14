//! Live probe: opens N Trouter connections the way the apps do and prints which
//! of them each chat event reaches. Reads ~/.cache/rv-teams-probe/tokens.json.
//! Prints ids and hashes only: no text, no name, no token.
//!   cargo run --example teams-live-probe -- [N] [seconds]   (from crates/rv-teams)

use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use rv_provider::socket::LiveEvent;
use rv_teams::auth::{Tokens, set_credential_sink};
use rv_teams::{Session, TeamsAccount};
use serde_json::Value;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

fn short(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:06x}", h.finish() & 0xff_ffff)
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(2);
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(300);
    let home = std::env::var("HOME").expect("HOME");
    let path = std::path::Path::new(&home).join(".cache/rv-teams-probe/tokens.json");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("tokens.json")).expect("json");
    let field = |k: &str| file[k].as_str().unwrap_or_default().to_owned();
    let info = Session {
        user_id: format!("8:orgid:{}", field("oid")),
        username: field("name"),
        account: TeamsAccount { tenant_id: field("tid"), refresh: Zeroizing::new(field("refresh_token")) },
    };
    let sink = path.clone();
    set_credential_sink(move |info: &Session| {
        let Ok(text) = std::fs::read_to_string(&sink) else { return };
        let Ok(mut doc) = serde_json::from_str::<Value>(&text) else { return };
        doc["refresh_token"] = Value::String(info.account.refresh.as_str().to_owned());
        let _ = std::fs::write(&sink, doc.to_string());
    });
    let tokens = Tokens::new(&info);
    let (all_tx, mut all_rx) = mpsc::unbounded_channel::<(usize, LiveEvent)>();
    let mut handles = Vec::new();
    for i in 0..n {
        let (handle, mut rx) = rv_teams::live::spawn(tokens.clone());
        handle.open();
        let tx = all_tx.clone();
        tokio::spawn(async move {
            while let Some(e) = rx.recv().await {
                let _ = tx.send((i, e));
            }
        });
        handles.push(handle);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let start = Instant::now();
    let mut seen: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            got = all_rx.recv() => {
                let Some((i, e)) = got else { break };
                let t = start.elapsed().as_secs();
                match e {
                    LiveEvent::State(s) => println!("{t:>4}s conn{i} state {s:?}"),
                    LiveEvent::Authenticated => println!("{t:>4}s conn{i} authenticated"),
                    LiveEvent::Lost => println!("{t:>4}s conn{i} LOST"),
                    LiveEvent::Event { name, data, .. } => {
                        let id = data["id"].as_str().or(data["clientmessageid"].as_str()).unwrap_or("?");
                        let conv = data["conversationLink"].as_str().unwrap_or("?");
                        let key = format!("{name} {} {}", short(conv), short(id));
                        println!("{t:>4}s conn{i} {key}");
                        seen.entry(key).or_default().push(i);
                    }
                }
            }
        }
    }
    println!("--- summary ({n} connections)");
    for (key, conns) in &seen {
        let mut c = conns.clone();
        c.sort();
        c.dedup();
        println!("{key}: {c:?}{}", if c.len() < n { "  <-- MISSED" } else { "" });
    }
    for h in handles {
        h.close();
    }
}
