//! Read-only probe of the activity feed, the `48:notifications` conversation.
//! Reads ~/.cache/rv-teams-probe/tokens.json (refresh_token, tid, oid, name),
//! walks up to ten pages back and prints shapes and counts only: no message
//! text, no name, no token.
//!   cargo run --example teams-feed        (from crates/rv-teams)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use reqwest::Method;
use rv_teams::auth::{Tokens, set_credential_sink};
use rv_teams::service::Service;
use rv_teams::translate::Directory;
use rv_teams::{Session, TeamsAccount};
use serde_json::Value;
use zeroize::Zeroizing;

fn kind_of(id: &str) -> String {
    let prefix = id.split(':').next().unwrap_or("");
    let suffix = id.rsplit_once('@').map(|(_, s)| s).unwrap_or("-");
    format!("{prefix}:…@{suffix}")
}

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").expect("HOME");
    let path = std::path::Path::new(&home).join(".cache/rv-teams-probe/tokens.json");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("tokens.json")).expect("json");
    let field = |k: &str| file[k].as_str().unwrap_or_default().to_owned();
    let me = format!("8:orgid:{}", field("oid"));
    let info = Session {
        user_id: me.clone(),
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
    let service = Service::new(Tokens::new(&info), &me, Arc::new(Directory::default()));

    let mut url = service
        .conversation_url("48:notifications", &["messages"], "pageSize=200&startTime=1&view=msnp24Equivalent")
        .await
        .expect("url");
    let mut all: Vec<Value> = Vec::new();
    for page in 0..10 {
        let answer = match service.chat(Method::GET, url.clone(), None).await {
            Ok(a) => a,
            Err(e) => {
                println!("page {page}: error status {} {:?}", e.status, e.error);
                break;
            }
        };
        let messages = answer.body["messages"].as_array().cloned().unwrap_or_default();
        let meta = &answer.body["_metadata"];
        let meta_keys: Vec<&String> = meta.as_object().map(|o| o.keys().collect()).unwrap_or_default();
        println!("page {page}: status {} messages {} _metadata {:?}", answer.status, messages.len(), meta_keys);
        all.extend(messages);
        let Some(back) = meta["backwardLink"].as_str().filter(|s| !s.is_empty()) else { break };
        let query = back.split_once('?').map(|(_, q)| q.to_owned()).unwrap_or_default();
        let query_keys: Vec<&str> = query.split('&').filter_map(|p| p.split('=').next()).collect();
        println!("  backwardLink query keys {query_keys:?}");
        let Some(next) = service.trusted(back).await else {
            println!("  backwardLink off the chat service, not followed");
            break;
        };
        url = next;
    }

    println!("total {}", all.len());
    if let (Some(first), Some(last)) = (all.first(), all.last()) {
        let day = |m: &Value| m["originalarrivaltime"].as_str().unwrap_or("").get(..10).unwrap_or("").to_owned();
        println!("newest {} oldest {}", day(first), day(last));
    }
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut keys_by_type: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut thread_kinds: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut chain: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut read: BTreeMap<String, usize> = BTreeMap::new();
    let mut top: BTreeSet<String> = BTreeSet::new();
    let mut prop_keys: BTreeSet<String> = BTreeSet::new();
    let mut target_me = (0, 0);
    let mut threads: BTreeSet<(String, String)> = BTreeSet::new();
    for m in &all {
        top.extend(m.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default());
        prop_keys.extend(m["properties"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default());
        let a = &m["properties"]["activity"];
        let t = format!(
            "{}/{}/{}",
            a["activityType"].as_str().unwrap_or("?"),
            a["activitySubtype"].as_str().unwrap_or("?"),
            a["sourceThreadType"].as_str().unwrap_or("?")
        );
        *by_type.entry(t.clone()).or_default() += 1;
        keys_by_type.entry(t.clone()).or_default().extend(a.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default());
        thread_kinds.entry(t.clone()).or_default().insert(kind_of(a["sourceThreadId"].as_str().unwrap_or("")));
        let entry = chain.entry(t.clone()).or_default();
        let rc = a["sourceReplyChainId"].as_u64().map(|n| n.to_string()).or_else(|| a["sourceReplyChainId"].as_str().map(str::to_owned));
        let mid = a["sourceMessageId"].as_u64().map(|n| n.to_string()).or_else(|| a["sourceMessageId"].as_str().map(str::to_owned));
        if let Some(rc) = &rc {
            entry.0 += 1;
            if Some(rc) == mid.as_ref() {
                entry.1 += 1;
            } else {
                entry.2 += 1;
            }
            threads.insert((a["sourceThreadId"].as_str().unwrap_or("").to_owned(), rc.clone()));
        }
        *read.entry(format!("{t} isread={}", m["properties"]["isread"])).or_default() += 1;
        if a["targetUserId"].as_str() == Some(me.as_str()) {
            target_me.0 += 1;
        } else {
            target_me.1 += 1;
        }
    }
    println!("message keys {top:?}");
    println!("properties keys {prop_keys:?}");
    println!("targetUserId is me: {} / other: {}", target_me.0, target_me.1);
    println!("distinct (thread, replyChain) pairs: {}", threads.len());
    for (t, n) in &by_type {
        let (with_rc, rc_is_msg, rc_other) = chain.get(t).copied().unwrap_or_default();
        println!("\n{t}: {n}");
        println!("  activity keys {:?}", keys_by_type[t]);
        println!("  source kinds {:?}", thread_kinds[t]);
        println!("  sourceReplyChainId: {with_rc} (equal to sourceMessageId {rc_is_msg}, different {rc_other})");
    }
    println!();
    for (k, n) in &read {
        println!("{k}: {n}");
    }
}
