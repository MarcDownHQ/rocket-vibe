//! The account's channel threads on Teams, read from its activity feed (the
//! `48:notifications` conversation): replies to my posts, replies in threads
//! I replied in, mentions, reactions. Chats have no threads. Teams' own
//! per-thread read state is not read: a thread is unread here when its newest
//! reply is past the store's `thread_seen` mark, or, never looked at, when
//! the feed holds one of its replies unread. Marking read moves that mark only.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;

use super::sync::TeamsSync;
use super::translate::{is_channel, message_id};
use crate::rest::RestError;
use crate::store::MessageRow;
use crate::thread_inbox::{Entry, Filter, Page, Participant, Totals};

const FEED: &str = "48:notifications";
const FEED_QUERY: &str = "pageSize=200&startTime=1&view=msnp24Equivalent";
const FEED_PAGES: usize = 5;
const FEED_FOR: Duration = Duration::from_secs(10);
const ROOT_FETCHES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Reply,
    Mention,
    Reaction,
}

/// One feed item about a channel thread.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    rid: String,
    chain: String,
    message: String,
    kind: Kind,
    at: i64,
    count: i64,
    read: bool,
    author_id: String,
    author_name: String,
    preview: String,
    topic: String,
}

/// A reply of the thread the feed told me about, its items merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: String,
    pub at: i64,
    pub count: i64,
    pub read: bool,
    pub mention: bool,
    pub replied: bool,
}

/// A channel thread as the feed tells it, newest item first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub rid: String,
    /// The root's message id (`<rid>/<reply chain id>`).
    pub root: String,
    pub topic: String,
    /// The newest item of any kind: a reaction moves the thread up too.
    pub last_activity: i64,
    /// Replies and mentions only, by message.
    pub replies: Vec<Reply>,
    /// The people who replied or mentioned me, newest first: (MRI, name).
    pub people: Vec<(String, String)>,
    /// The newest reply's preview and author (MRI, name), standing in for an unreachable root.
    pub preview: Option<(String, String, String)>,
}

impl Thread {
    /// Someone replied to my post or in a thread I replied in.
    pub fn participated(&self) -> bool {
        self.replies.iter().any(|r| r.replied)
    }

    pub fn mentioned(&self) -> bool {
        self.replies.iter().any(|r| r.mention)
    }

    /// The feed's sum of replies: those that concern me, a lower bound.
    pub fn reply_count(&self) -> i64 {
        self.replies.iter().map(|r| r.count).sum()
    }

    /// The newest reply or mention, or the newest activity without one.
    pub fn last_reply(&self) -> i64 {
        self.replies.iter().map(|r| r.at).max().unwrap_or(self.last_activity)
    }

    /// (unread replies, unread mentions) past `seen`, or by the feed's own
    /// read flags when the thread was never looked at here.
    pub fn unread(&self, seen: Option<i64>) -> (i64, i64) {
        let unread: Vec<&Reply> = self.replies.iter().filter(|r| seen.map_or(!r.read, |seen| r.at > seen)).collect();
        if unread.is_empty() {
            return (0, 0);
        }
        let replies = unread.iter().map(|r| r.count).sum::<i64>().max(1);
        (replies, unread.iter().filter(|r| r.mention).count() as i64)
    }
}

fn text_of(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn read_flag(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.eq_ignore_ascii_case("false"),
        _ => true,
    }
}

fn instant(v: &Value, key: &str) -> Option<i64> {
    let raw = v.get(key)?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|t| t.timestamp_millis())
}

fn item(raw: &Value) -> Option<Item> {
    let props = raw.get("properties")?;
    let activity = match props.get("activity")? {
        Value::String(json) => serde_json::from_str(json).ok()?,
        other => other.clone(),
    };
    let rid = text_of(&activity, "sourceThreadId").filter(|rid| is_channel(rid))?;
    let message = text_of(&activity, "sourceMessageId")?;
    let chain = text_of(&activity, "sourceReplyChainId").filter(|chain| *chain != message && chain != "0")?;
    let kind = match text_of(&activity, "activityType")?.as_str() {
        "reply" | "replyToReply" => Kind::Reply,
        "mention" if text_of(&activity, "activitySubtype").as_deref() == Some("person") => Kind::Mention,
        "reaction" => Kind::Reaction,
        _ => return None,
    };
    let at = instant(&activity, "activityTimestamp")
        .or_else(|| instant(raw, "originalarrivaltime"))
        .or_else(|| message.parse().ok())?;
    Some(Item {
        rid,
        chain,
        message,
        kind,
        at,
        count: activity.get("count").and_then(|c| c.as_i64().or_else(|| c.as_str()?.parse().ok())).unwrap_or(1).max(1),
        read: read_flag(props.get("isread")),
        author_id: text_of(&activity, "sourceUserId").unwrap_or_default(),
        author_name: text_of(&activity, "sourceUserImDisplayName").unwrap_or_default(),
        preview: text_of(&activity, "messagePreview").unwrap_or_default(),
        topic: text_of(&activity, "sourceThreadTopic").unwrap_or_default(),
    })
}

/// The feed's channel threads, newest activity first. `me` is left out of the people.
pub fn group(feed: &[Value], me: &str) -> Vec<Thread> {
    let mut items: Vec<Item> = feed.iter().filter_map(item).collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.at));
    let mut order: Vec<String> = Vec::new();
    let mut threads: HashMap<String, Thread> = HashMap::new();
    for item in items {
        let root = message_id(&item.rid, &item.chain);
        let thread = threads.entry(root.clone()).or_insert_with(|| {
            order.push(root.clone());
            Thread {
                rid: item.rid.clone(),
                root,
                topic: String::new(),
                last_activity: item.at,
                replies: Vec::new(),
                people: Vec::new(),
                preview: None,
            }
        });
        if thread.topic.is_empty() {
            thread.topic = item.topic.clone();
        }
        if item.kind == Kind::Reaction {
            continue;
        }
        if thread.preview.is_none() && !item.preview.is_empty() {
            thread.preview = Some((item.author_id.clone(), item.author_name.clone(), item.preview.clone()));
        }
        if !item.author_id.is_empty()
            && item.author_id != me
            && !thread.people.iter().any(|(id, _)| *id == item.author_id)
        {
            thread.people.push((item.author_id.clone(), item.author_name.clone()));
        }
        let id = message_id(&item.rid, &item.message);
        let mention = item.kind == Kind::Mention;
        match thread.replies.iter_mut().find(|r| r.id == id) {
            Some(reply) => {
                reply.count = reply.count.max(item.count);
                reply.read &= item.read;
                reply.mention |= mention;
                reply.replied |= !mention;
            }
            None => thread.replies.push(Reply {
                id,
                at: item.at,
                count: item.count,
                read: item.read,
                mention,
                replied: !mention,
            }),
        }
    }
    order.into_iter().filter_map(|root| threads.remove(&root)).collect()
}

/// The feed, read again at most every `FEED_FOR`: the totals and the page
/// asked together share one read.
async fn feed(sync: &TeamsSync) -> Result<Arc<Vec<Value>>, RestError> {
    let mut cached = sync.feed.lock().await;
    if let Some((at, feed)) = cached.as_ref()
        && at.elapsed() < FEED_FOR
    {
        return Ok(feed.clone());
    }
    let (mut all, mut back) = sync.service.messages(FEED, FEED_QUERY).await?;
    for _ in 1..FEED_PAGES {
        let Some(url) = back.take().filter(|l| !l.is_empty()) else { break };
        let Some(url) = sync.service.trusted(&url).await else { break };
        let (page, older) = sync.service.page(url).await?;
        if page.is_empty() {
            break;
        }
        all.extend(page);
        back = older;
    }
    let feed = Arc::new(all);
    *cached = Some((Instant::now(), feed.clone()));
    Ok(feed)
}

async fn threads(sync: &TeamsSync) -> Result<Vec<Thread>, RestError> {
    let threads = group(&feed(sync).await?, sync.me());
    for (mri, name) in threads.iter().flat_map(|t| t.people.iter()) {
        if !name.is_empty() && !sync.directory.known(mri) {
            sync.directory.remember(mri, name);
        }
    }
    Ok(threads)
}

/// Stores the roots the store does not hold yet, once a session each.
async fn fetch_roots(sync: &TeamsSync, threads: &[Thread]) {
    let ids: Vec<String> = threads.iter().map(|t| t.root.clone()).collect();
    let stored: Vec<String> = sync.store().messages_by_id(&ids).into_iter().map(|r| r.id).collect();
    let missing: Vec<String> = {
        let unreachable = sync.unreachable_roots.lock().unwrap();
        ids.into_iter().filter(|id| !stored.contains(id) && !unreachable.contains(id)).collect()
    };
    let fetched: Vec<(String, Result<Option<crate::normalize::Message>, RestError>)> =
        futures_util::stream::iter(missing)
            .map(|id| async move {
                let message = sync.fetch_message(&id).await;
                (id, message)
            })
            .buffer_unordered(ROOT_FETCHES)
            .collect()
            .await;
    let mut unreachable = Vec::new();
    sync.store().write(|w| {
        for (id, message) in &fetched {
            match message {
                Ok(Some(message)) => w.upsert_message(message),
                _ if root_gone(message) => unreachable.push(id.clone()),
                _ => {}
            }
        }
    });
    sync.unreachable_roots.lock().unwrap().extend(unreachable);
}

/// The service will not hand this root out: asking again this session is useless.
fn root_gone<T>(answer: &Result<Option<T>, RestError>) -> bool {
    match answer {
        Ok(found) => found.is_none(),
        Err(e) => (400..500).contains(&e.status) && !matches!(e.status, 401 | 408 | 429),
    }
}

/// The root as the feed tells it, when the service would not hand it out.
fn preview_row(thread: &Thread) -> MessageRow {
    let (author_id, author, text) = thread.preview.clone().unwrap_or_default();
    let ts = super::translate::split_id(&thread.root)
        .and_then(|(_, id)| id.parse().ok())
        .unwrap_or_else(|| thread.last_reply());
    MessageRow {
        id: thread.root.clone(),
        rid: thread.rid.clone(),
        ts,
        text: Some(text),
        author: (!author.is_empty()).then_some(author),
        author_id,
        ..Default::default()
    }
}

fn entry(sync: &TeamsSync, thread: &Thread, root: Option<MessageRow>) -> Entry {
    let root = root.unwrap_or_else(|| preview_row(thread));
    let room = sync.store().room_name(&thread.rid).map(|(name, _)| name).unwrap_or_else(|| thread.topic.clone());
    let (unread_replies, unread_mentions) = thread.unread(sync.store().thread_seen_at(&thread.root));
    let participants = thread
        .people
        .iter()
        .map(|(id, name)| Participant { id: id.clone(), username: name.clone(), label: name.clone(), photo: None })
        .collect();
    Entry {
        room,
        last_reply: thread.last_activity,
        replies: thread.reply_count().max(root.thread_count),
        participants,
        unread_replies,
        unread_mentions,
        participated: thread.participated() || root.author_id == sync.me(),
        mentioned: thread.mentioned(),
        root,
    }
}

async fn entries(sync: &TeamsSync) -> Result<Vec<Entry>, RestError> {
    let threads = threads(sync).await?;
    fetch_roots(sync, &threads).await;
    let ids: Vec<String> = threads.iter().map(|t| t.root.clone()).collect();
    let mut roots: HashMap<String, MessageRow> =
        sync.store().messages_by_id(&ids).into_iter().map(|r| (r.id.clone(), r)).collect();
    Ok(threads.iter().map(|thread| entry(sync, thread, roots.remove(&thread.root))).collect())
}

/// The whole list at once: the feed is one short page.
pub async fn page(sync: &TeamsSync, filter: Filter, cursor: Option<&str>) -> Result<Page, RestError> {
    if cursor.is_some() {
        return Ok(Page::default());
    }
    let mut entries = entries(sync).await?;
    if filter == Filter::Unread {
        entries.retain(|e| e.unread_replies > 0);
    }
    Ok(Page { entries, next: None })
}

pub async fn totals(sync: &TeamsSync) -> Result<Totals, RestError> {
    let threads = threads(sync).await?;
    let mut totals = Totals::default();
    for thread in &threads {
        let (replies, mentions) = thread.unread(sync.store().thread_seen_at(&thread.root));
        if replies > 0 {
            totals.unread_threads += 1;
            totals.unread_mentions += mentions;
        }
    }
    Ok(totals)
}

fn mark(sync: &TeamsSync, rid: &str, root: &str, last_reply: i64) {
    let now = chrono::Utc::now().timestamp_millis();
    sync.store().write(|w| w.thread_seen(rid, root, now.max(last_reply)));
}

/// Reads the thread up to now, here only. True when the list it was on changed.
pub async fn mark_read(sync: &TeamsSync, rid: &str, root: &str) -> bool {
    let cached = sync.feed.lock().await.as_ref().map(|(_, feed)| feed.clone());
    let thread = cached.and_then(|feed| group(&feed, sync.me()).into_iter().find(|t| t.root == root));
    let unread = thread.as_ref().is_some_and(|t| t.unread(sync.store().thread_seen_at(root)).0 > 0);
    mark(sync, rid, root, thread.map_or(0, |t| t.last_reply()));
    unread
}

pub async fn mark_all_read(sync: &TeamsSync) -> Result<(), RestError> {
    for thread in threads(sync).await? {
        mark(sync, &thread.rid, &thread.root, thread.last_reply());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHANNEL: &str = "19:alpha@thread.tacv2";
    const OTHER: &str = "19:beta@thread.tacv2";
    const CHAT: &str = "19:gamma@thread.v2";
    const ME: &str = "8:orgid:me";

    fn feed_item(kind: &str, sub: &str, rid: &str, chain: Value, message: Value, at: &str, read: Value) -> Value {
        json!({
            "id": "1",
            "originalarrivaltime": at,
            "properties": {
                "isread": read,
                "activity": {
                    "activityType": kind,
                    "activitySubtype": sub,
                    "activityTimestamp": at,
                    "sourceThreadId": rid,
                    "sourceMessageId": message,
                    "sourceReplyChainId": chain,
                    "sourceThreadType": "topic",
                    "sourceThreadTopic": "Planning",
                    "sourceUserId": "8:orgid:ada",
                    "sourceUserImDisplayName": "Ada Example",
                    "messagePreview": "sounds good",
                    "count": 1,
                    "targetUserId": ME,
                }
            }
        })
    }

    fn ms(at: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(at).unwrap().timestamp_millis()
    }

    #[test]
    fn channel_replies_group_by_reply_chain_and_chats_are_skipped() {
        let feed = vec![
            feed_item("replyToReply", "", CHANNEL, json!("100"), json!("300"), "2026-11-02T10:00:00Z", json!("true")),
            feed_item("reply", "", CHANNEL, json!(100), json!(200), "2026-11-01T10:00:00Z", json!(false)),
            feed_item("reply", "", OTHER, json!("500"), json!("600"), "2026-11-03T10:00:00Z", json!(true)),
            feed_item(
                "mentionInChat",
                "person",
                CHAT,
                json!("700"),
                json!("700"),
                "2026-11-04T10:00:00Z",
                json!(false),
            ),
            feed_item("reactionInChat", "like", CHAT, json!("700"), json!("700"), "2026-11-04T10:00:00Z", json!(false)),
            feed_item("mention", "person", OTHER, json!("800"), json!("800"), "2026-11-05T10:00:00Z", json!(false)),
            feed_item("call", "missedCall", CHANNEL, json!("100"), json!("900"), "2026-11-05T10:00:00Z", json!(false)),
        ];
        let threads = group(&feed, ME);
        assert_eq!(
            threads.iter().map(|t| t.root.as_str()).collect::<Vec<_>>(),
            [format!("{OTHER}/500"), format!("{CHANNEL}/100")]
        );
        let alpha = &threads[1];
        assert_eq!(alpha.rid, CHANNEL);
        assert_eq!(alpha.replies.len(), 2);
        assert_eq!(alpha.last_reply(), ms("2026-11-02T10:00:00Z"));
        assert_eq!(alpha.reply_count(), 2);
        assert!(alpha.participated());
        assert!(!alpha.mentioned());
        assert_eq!(alpha.people, [("8:orgid:ada".to_owned(), "Ada Example".to_owned())]);
        assert_eq!(alpha.topic, "Planning");
    }

    #[test]
    fn a_mention_in_a_thread_is_mentioned_and_merges_with_its_reply() {
        let feed = vec![
            feed_item("mention", "person", CHANNEL, json!("100"), json!("300"), "2026-11-02T10:00:00Z", json!("false")),
            feed_item("replyToReply", "", CHANNEL, json!("100"), json!("300"), "2026-11-02T10:00:00Z", json!("true")),
            feed_item("mention", "channel", CHANNEL, json!("100"), json!("400"), "2026-11-03T10:00:00Z", json!(false)),
        ];
        let threads = group(&feed, ME);
        assert_eq!(threads.len(), 1);
        let thread = &threads[0];
        assert_eq!(thread.replies.len(), 1, "one reply, two items");
        assert!(thread.mentioned() && thread.participated());
        assert_eq!(thread.unread(None), (1, 1), "read only when every item of the reply is");
    }

    #[test]
    fn a_reaction_moves_the_thread_up_and_is_neither_reply_nor_chip() {
        let feed = vec![
            feed_item("reaction", "like", CHANNEL, json!("100"), json!("300"), "2026-11-05T10:00:00Z", json!(false)),
            feed_item("reply", "", CHANNEL, json!("100"), json!("200"), "2026-11-01T10:00:00Z", json!(true)),
            feed_item("reaction", "like", OTHER, json!("500"), json!("600"), "2026-11-04T10:00:00Z", json!(false)),
            feed_item("reaction", "like", OTHER, json!("700"), json!("700"), "2026-11-06T10:00:00Z", json!(false)),
        ];
        let threads = group(&feed, ME);
        assert_eq!(threads.len(), 2);
        let alpha = threads.iter().find(|t| t.rid == CHANNEL).unwrap();
        assert_eq!(alpha.last_activity, ms("2026-11-05T10:00:00Z"));
        assert_eq!(alpha.last_reply(), ms("2026-11-01T10:00:00Z"));
        assert_eq!(alpha.reply_count(), 1);
        assert_eq!(alpha.unread(None), (0, 0));
        let beta = threads.iter().find(|t| t.rid == OTHER).unwrap();
        assert!(!beta.participated() && !beta.mentioned());
        assert_eq!(beta.unread(None), (0, 0));
        assert_eq!(threads[0].rid, CHANNEL, "newest activity first");
    }

    #[test]
    fn unread_follows_the_seen_mark_then_the_feed_flags() {
        let mut later =
            feed_item("replyToReply", "", CHANNEL, json!("100"), json!("300"), "2026-11-02T10:00:00Z", json!(true));
        later["properties"]["activity"]["count"] = json!("3");
        let feed = vec![
            later,
            feed_item("reply", "", CHANNEL, json!("100"), json!("200"), "2026-11-01T10:00:00Z", json!("false")),
        ];
        let thread = &group(&feed, ME)[0];
        assert_eq!(thread.reply_count(), 4);
        assert_eq!(thread.unread(None), (1, 0), "no mark: the feed's unread item");
        assert_eq!(thread.unread(Some(ms("2026-11-01T12:00:00Z"))), (3, 0), "past the mark");
        assert_eq!(thread.unread(Some(ms("2026-11-02T10:00:00Z"))), (0, 0));
    }

    #[test]
    fn me_is_not_a_participant_and_an_activity_may_be_a_string() {
        let mut mine = feed_item("reply", "", CHANNEL, json!("100"), json!("200"), "2026-11-01T10:00:00Z", json!(true));
        mine["properties"]["activity"]["sourceUserId"] = json!(ME);
        let mut text = feed_item("reply", "", CHANNEL, json!("100"), json!("300"), "2026-11-02T10:00:00Z", Value::Null);
        let activity = text["properties"]["activity"].to_string();
        text["properties"]["activity"] = Value::String(activity);
        let thread = &group(&[mine, text], ME)[0];
        assert_eq!(thread.replies.len(), 2);
        assert_eq!(thread.people.len(), 1);
        assert_eq!(thread.unread(None), (0, 0), "an absent flag is read");
    }

    #[test]
    fn a_root_the_service_would_not_hand_out_shows_the_newest_reply() {
        let feed = vec![feed_item(
            "reply",
            "",
            CHANNEL,
            json!("1762000000000"),
            json!("1762000001000"),
            "2026-11-01T10:00:00Z",
            json!(true),
        )];
        let row = preview_row(&group(&feed, ME)[0]);
        assert_eq!(row.id, format!("{CHANNEL}/1762000000000"));
        assert_eq!(row.rid, CHANNEL);
        assert_eq!(row.ts, 1762000000000);
        assert_eq!(row.text.as_deref(), Some("sounds good"));
        assert_eq!(row.author.as_deref(), Some("Ada Example"));
    }

    #[test]
    fn only_a_root_the_service_refuses_is_given_up_on() {
        let status = |status: u16| -> Result<Option<()>, RestError> {
            Err(RestError { status, ..RestError::network("down".to_owned()) })
        };
        assert!(root_gone::<()>(&Ok(None)));
        assert!(!root_gone(&Ok(Some(()))));
        assert!(root_gone(&status(403)));
        assert!(root_gone(&status(400)));
        for transient in [0, 401, 408, 429, 500, 503] {
            assert!(!root_gone(&status(transient)), "{transient} is asked again");
        }
    }
}
