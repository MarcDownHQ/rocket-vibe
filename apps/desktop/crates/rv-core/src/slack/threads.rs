//! The account's threads on Slack: `subscriptions.thread.*`, which lists the
//! threads I follow by latest reply. Slack has no unread list: it is walked
//! here, comparing each root's `latest_reply` with its `last_read`.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;

use super::sync::SlackSync;
use super::translate::{message_id, split_id, ts_ms};
use crate::rest::RestError;
use crate::store::MessageRow;
use crate::thread_inbox::{Entry, Filter, Page, Participant, Totals, participated};
use rv_slack::service::{is, list, text};

const PAGE: usize = 25;
const WALK_PAGE: usize = 50;
const WALK_PAGES: usize = 4;

/// The exact `ts` of each followed thread's latest reply, and how far each
/// was marked: the store only keeps milliseconds, and a mark never goes back.
#[derive(Default)]
pub struct Marks {
    latest: Mutex<HashMap<String, String>>,
    marked: Mutex<HashMap<String, String>>,
}

impl Marks {
    fn latest(&self, root: &str) -> Option<String> {
        self.latest.lock().unwrap().get(root).cloned()
    }

    fn already_marked(&self, root: &str, ts: &str) -> bool {
        self.marked.lock().unwrap().get(root).is_some_and(|marked| !newer(ts, marked))
    }
}

fn valid(ts: &str) -> bool {
    ts_ms(ts).is_some()
}

fn newer(a: &str, b: &str) -> bool {
    valid(a) && (!valid(b) || super::compare_timestamp(a, b).is_gt())
}

fn root_msg(thread: &Value) -> Option<&Value> {
    thread.get("root_msg")
}

fn replies(thread: &Value) -> Vec<Value> {
    list(thread, "latest_replies")
}

fn root_id(root: &Value) -> Option<String> {
    let ts = text(root, "thread_ts").or_else(|| text(root, "ts"))?;
    Some(message_id(text(root, "channel")?, ts))
}

fn is_unread(root: &Value) -> bool {
    match (text(root, "latest_reply"), text(root, "last_read")) {
        (Some(latest), Some(read)) => newer(latest, read),
        _ => false,
    }
}

fn names_me(message: &Value, me: &str) -> bool {
    let body = text(message, "text").unwrap_or_default();
    body.contains(&format!("<@{me}>")) || body.contains(&format!("<@{me}|"))
}

struct View {
    threads: Vec<Value>,
    next: Option<String>,
}

async fn view(sync: &SlackSync, limit: usize, cursor: Option<&str>) -> Result<View, RestError> {
    let mut params = vec![("limit", limit.to_string()), ("priority_mode", "all".to_owned())];
    if let Some(cursor) = cursor {
        params.push(("current_ts", cursor.to_owned()));
    }
    let body = sync.service.call("subscriptions.thread.getView", params).await?;
    let threads = list(&body, "threads");
    {
        let mut latest = sync.thread_marks.latest.lock().unwrap();
        for root in threads.iter().filter_map(root_msg) {
            if let (Some(id), Some(ts)) = (root_id(root), text(root, "latest_reply")) {
                latest.insert(id, ts.to_owned());
            }
        }
    }
    let last = threads.last().and_then(root_msg).and_then(|root| text(root, "latest_reply"));
    let next = last.filter(|ts| is(&body, "has_more") && cursor != Some(*ts)).map(str::to_owned);
    Ok(View { threads, next })
}

async fn entries(sync: &SlackSync, threads: &[Value]) -> Vec<Entry> {
    let mut messages: Vec<Value> = threads.iter().filter_map(root_msg).cloned().collect();
    messages.extend(threads.iter().flat_map(replies));
    sync.service.resolve(&messages).await;
    let people = threads.iter().filter_map(root_msg).flat_map(|root| list(root, "reply_users"));
    sync.service.ensure_people(people.filter_map(|id| id.as_str().map(str::to_owned)).collect::<Vec<_>>()).await;
    threads.iter().filter_map(|thread| entry(sync, thread)).collect()
}

fn entry(sync: &SlackSync, thread: &Value) -> Option<Entry> {
    let raw = root_msg(thread)?;
    let channel = text(raw, "channel")?;
    let root = MessageRow::from(&sync.translate(channel, raw)?);
    let later = replies(thread);
    let me = sync.me();
    let read = text(raw, "last_read").unwrap_or("0");
    let unread: Vec<&Value> =
        later.iter().filter(|reply| text(reply, "ts").is_some_and(|ts| is_unread(raw) && newer(ts, read))).collect();
    let (room, kind) = sync.store().room_name(channel).map_or((String::new(), None), |(n, k)| (n, Some(k)));
    let repliers: Vec<String> = list(raw, "reply_users").iter().filter_map(|u| u.as_str().map(str::to_owned)).collect();
    let writers = later.iter().filter_map(|reply| text(reply, "user"));
    let took_part =
        participated(me, &root.author_id, repliers.iter().map(String::as_str).chain(writers), kind.as_deref());
    let participants = repliers
        .iter()
        .filter_map(|id| {
            let username = sync.directory.username(id)?;
            let label = sync.directory.display_name(id).unwrap_or_default();
            Some(Participant { id: id.clone(), username, label, photo: None })
        })
        .collect();
    let unread_mentions = unread.iter().filter(|reply| names_me(reply, me)).count() as i64;
    Some(Entry {
        room,
        last_reply: text(raw, "latest_reply").and_then(ts_ms).unwrap_or(root.ts),
        replies: raw.get("reply_count").and_then(Value::as_i64).unwrap_or(root.thread_count),
        participants,
        unread_replies: if is_unread(raw) { (unread.len() as i64).max(1) } else { 0 },
        unread_mentions,
        participated: took_part,
        mentioned: names_me(raw, me) || later.iter().any(|reply| names_me(reply, me)),
        root,
    })
}

/// One page. The unread list has no server filter: pages are read until enough
/// threads turn out unread, at most `WALK_PAGES` at a time, and the cursor goes
/// on from there even when this page came back short or empty.
pub async fn page(sync: &SlackSync, filter: Filter, cursor: Option<&str>) -> Result<Page, RestError> {
    if filter == Filter::Followed {
        let view = view(sync, PAGE, cursor).await?;
        return Ok(Page { entries: entries(sync, &view.threads).await, next: view.next });
    }
    let mut found = Vec::new();
    let mut position = cursor.map(str::to_owned);
    for _ in 0..WALK_PAGES {
        let view = view(sync, WALK_PAGE, position.as_deref()).await?;
        found.extend(view.threads.into_iter().filter(|t| root_msg(t).is_some_and(is_unread)));
        position = view.next;
        if position.is_none() || found.len() >= PAGE {
            break;
        }
    }
    Ok(Page { entries: entries(sync, &found).await, next: position })
}

/// The mentions from `client.counts`; the threads counted over one page of the
/// list, which is as far as a badge should read.
pub async fn totals(sync: &SlackSync) -> Result<Totals, RestError> {
    let (counts, view) = tokio::join!(
        sync.service.call("client.counts", vec![("thread_counts_by_channel", "true".to_owned())]),
        view(sync, WALK_PAGE, None)
    );
    let unread_threads = view?.threads.iter().filter_map(root_msg).filter(|root| is_unread(root)).count() as i64;
    let threads = counts?.get("threads").cloned().unwrap_or(Value::Null);
    let unread_mentions = threads.get("mention_count").and_then(Value::as_i64).unwrap_or(0);
    Ok(Totals { unread_threads, unread_mentions })
}

/// The newest reply known of a thread: the list's, or a stored one that came
/// live since; read from Slack when neither knows.
async fn latest_reply(sync: &SlackSync, rid: &str, root: &str, thread_ts: &str) -> Result<Option<String>, RestError> {
    let stored = sync.store().thread_messages(root).into_iter().filter_map(|m| {
        let (_, ts) = split_id(&m.id)?;
        Some(ts.to_owned())
    });
    let known = stored.chain(sync.thread_marks.latest(root)).reduce(|a, b| if newer(&b, &a) { b } else { a });
    if let Some(ts) = known.filter(|ts| ts != thread_ts) {
        return Ok(Some(ts));
    }
    let messages = sync.service.thread(rid, thread_ts).await?;
    Ok(messages.last().and_then(|m| text(m, "ts")).filter(|ts| *ts != thread_ts).map(str::to_owned))
}

/// Marks to the latest reply, forward only, and only a thread I follow: what
/// the mark does to any other is not known. Whether a mark was sent.
pub async fn mark_read(sync: &SlackSync, rid: &str, root: &str) -> Result<bool, RestError> {
    let Some((_, thread_ts)) = split_id(root) else {
        return Err(RestError::incomplete("thread: not a Slack id"));
    };
    if sync.thread_marks.latest(root).is_none() && !following(sync, root).await? {
        return Ok(false);
    }
    let Some(ts) = latest_reply(sync, rid, root, thread_ts).await? else { return Ok(false) };
    if sync.thread_marks.already_marked(root, &ts) {
        return Ok(false);
    }
    let params = vec![("channel", rid.to_owned()), ("thread_ts", thread_ts.to_owned()), ("ts", ts.clone())];
    sync.service.call("subscriptions.thread.mark", params).await?;
    sync.thread_marks.marked.lock().unwrap().insert(root.to_owned(), ts);
    Ok(true)
}

pub async fn mark_all_read(sync: &SlackSync) -> Result<(), RestError> {
    sync.service.call("subscriptions.thread.clearAll", Vec::new()).await?;
    let mut marks = sync.thread_marks.marked.lock().unwrap();
    let latest = sync.thread_marks.latest.lock().unwrap();
    marks.extend(latest.iter().map(|(root, ts)| (root.clone(), ts.clone())));
    Ok(())
}

pub async fn follow(sync: &SlackSync, root: &str, on: bool) -> Result<(), RestError> {
    let Some((channel, thread_ts)) = split_id(root) else {
        return Err(RestError::incomplete("thread: not a Slack id"));
    };
    let method = if on { "subscriptions.thread.add" } else { "subscriptions.thread.remove" };
    sync.service.call(method, vec![("channel", channel.to_owned()), ("thread_ts", thread_ts.to_owned())]).await?;
    if !on {
        sync.thread_marks.latest.lock().unwrap().remove(root);
    }
    Ok(())
}

/// Followed when the answer lists the thread: the empty answer was not recorded
/// when it was probed, so nothing else is read into it.
pub async fn following(sync: &SlackSync, root: &str) -> Result<bool, RestError> {
    let Some((channel, thread_ts)) = split_id(root) else {
        return Err(RestError::incomplete("thread: not a Slack id"));
    };
    let params = vec![("channel", channel.to_owned()), ("thread_ts", thread_ts.to_owned())];
    let body = sync.service.call("subscriptions.thread.get", params).await?;
    Ok(list(&body, "subscriptions").iter().any(|ts| ts.as_str() == Some(thread_ts)))
}

/// The thread a live message frame belongs to, edits and reply notices included.
pub fn frame_root(frame: &Value) -> Option<String> {
    let channel = text(frame, "channel")?;
    let carried = [Some(frame), frame.get("message"), frame.get("previous_message")];
    let ts = carried.into_iter().flatten().find_map(|m| text(m, "thread_ts"))?;
    Some(message_id(channel, ts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unread_compares_the_exact_timestamps() {
        let root = |latest: &str, read: &str| json!({"latest_reply": latest, "last_read": read});
        assert!(is_unread(&root("1700000050.000200", "1700000050.000100")));
        assert!(!is_unread(&root("1700000050.000100", "1700000050.000100")));
        assert!(!is_unread(&root("1700000040.000100", "1700000050.000100")));
        assert!(!is_unread(&json!({"latest_reply": "1700000050.000100"})), "no last_read: not known unread");
        assert!(!is_unread(&root("garbage", "1700000050.000100")), "never panics on odd values");
    }

    #[test]
    fn a_mention_is_my_own_id() {
        assert!(names_me(&json!({"text": "hi <@U000>"}), "U000"));
        assert!(names_me(&json!({"text": "hi <@U000|me>"}), "U000"));
        assert!(!names_me(&json!({"text": "hi <@U0001> <!here>"}), "U000"));
    }

    #[test]
    fn a_frame_carries_its_thread_itself_or_in_its_nested_message() {
        let direct = json!({"channel": "C1", "ts": "2.000000", "thread_ts": "1.000000"});
        assert_eq!(frame_root(&direct).as_deref(), Some("C1:1.000000"));
        let edited = json!({"channel": "C1", "subtype": "message_changed", "message": {"thread_ts": "1.000000"}});
        assert_eq!(frame_root(&edited).as_deref(), Some("C1:1.000000"));
        assert_eq!(frame_root(&json!({"channel": "C1", "ts": "2.000000"})), None);
    }
}
