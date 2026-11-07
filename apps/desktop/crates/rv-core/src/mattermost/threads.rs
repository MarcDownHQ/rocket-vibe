//! The account's threads on Mattermost and kChat. The server lists them per
//! team, each list holding the DMs and groups as well, so several teams are
//! merged and deduplicated here.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use super::sync::MmSync;
use crate::rest::{CallOptions, RestClient, RestError};
use crate::thread_inbox::{Entry, Filter, Page, Participant, Totals, names, participated};

const PAGE: usize = 25;
const TOTALS_PAGE: usize = 100;

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

async fn teams(rest: &RestClient) -> Result<Vec<String>, RestError> {
    let teams = rest.get("users/me/teams", CallOptions::default()).await?;
    Ok(teams.as_array().into_iter().flatten().filter_map(|t| text(t, "id")).map(str::to_owned).collect())
}

/// Each team's position, empty for its start, the teams with nothing left
/// being absent.
fn parse_cursor(cursor: &str) -> BTreeMap<String, String> {
    cursor
        .split(',')
        .filter_map(|part| part.split_once(':'))
        .map(|(team, before)| (team.to_owned(), before.to_owned()))
        .collect()
}

fn print_cursor(cursor: &BTreeMap<String, String>) -> Option<String> {
    (!cursor.is_empty()).then(|| cursor.iter().map(|(t, b)| format!("{t}:{b}")).collect::<Vec<_>>().join(","))
}

/// One team's page as the server answered it.
pub(crate) struct TeamPage {
    pub team: String,
    pub before: Option<String>,
    pub threads: Vec<Value>,
    pub full: bool,
}

/// Merges by last reply, newest first. A team with more to read limits what
/// can be shown: anything older than its last entry might sort after what its
/// next page brings, so it waits for that page.
pub(crate) fn merge(pages: Vec<TeamPage>) -> (Vec<Value>, Option<String>) {
    let floor = pages
        .iter()
        .filter(|p| p.full)
        .filter_map(|p| p.threads.last())
        .map(|t| int(t, "last_reply_at"))
        .max()
        .unwrap_or(i64::MIN);
    let mut next = BTreeMap::new();
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for page in pages {
        let shown: Vec<&Value> = page.threads.iter().filter(|t| int(t, "last_reply_at") >= floor).collect();
        if page.full || shown.len() < page.threads.len() {
            let before = shown.last().and_then(|t| text(t, "id")).map(str::to_owned).or(page.before);
            next.insert(page.team.clone(), before.unwrap_or_default());
        }
        for thread in shown {
            if text(thread, "id").is_some_and(|id| seen.insert(id.to_owned())) {
                merged.push(thread.clone());
            }
        }
    }
    merged.sort_by_key(|t| std::cmp::Reverse(int(t, "last_reply_at")));
    (merged, print_cursor(&next))
}

fn participants(sync: &MmSync, thread: &Value) -> Vec<Participant> {
    thread
        .get("participants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|person| {
            let id = text(person, "id")?.to_owned();
            let username = text(person, "username").map(str::to_owned).or_else(|| sync.directory.username(&id))?;
            let label = sync.directory.display_name(&id).unwrap_or_default();
            Some(Participant { id, username, label, photo: None })
        })
        .collect()
}

fn took_part(me: &str, author: &str, thread: &Value, room_kind: Option<&str>) -> bool {
    let repliers = thread.get("participants").and_then(Value::as_array).into_iter().flatten();
    participated(me, author, repliers.filter_map(|person| text(person, "id")), room_kind)
}

pub async fn page(sync: &MmSync, filter: Filter, cursor: Option<&str>) -> Result<Page, RestError> {
    let rest = sync.rest();
    let known = cursor.map(parse_cursor);
    let mut pages = Vec::new();
    for team in teams(rest).await? {
        let before = match &known {
            Some(known) => match known.get(&team) {
                Some(before) => Some(before.clone()).filter(|b| !b.is_empty()),
                None => continue,
            },
            None => None,
        };
        let mut query = vec![("per_page", PAGE.to_string()), ("extended", "true".to_owned())];
        if filter == Filter::Unread {
            query.push(("unread", "true".to_owned()));
        }
        if let Some(before) = &before {
            query.push(("before", before.clone()));
        }
        let answer = rest.get(&format!("users/me/teams/{team}/threads"), CallOptions::params(query)).await?;
        let threads = answer.get("threads").and_then(Value::as_array).cloned().unwrap_or_default();
        let full = threads.len() >= PAGE;
        pages.push(TeamPage { team, before, threads, full });
    }
    let (threads, next) = merge(pages);
    let posts: Vec<Value> = threads.iter().filter_map(|t| t.get("post").cloned()).collect();
    sync.ensure_authors(&posts).await;
    let people: Vec<String> = threads
        .iter()
        .filter_map(|t| t.get("participants").and_then(Value::as_array))
        .flatten()
        .filter_map(|person| text(person, "id"))
        .map(str::to_owned)
        .collect();
    sync.directory.ensure(rest, people).await;
    sync.ingest(&posts);
    let ids: Vec<String> = threads.iter().filter_map(|t| text(t, "id")).map(str::to_owned).collect();
    let rows = sync.store().messages_by_id(&ids);
    let entries = threads
        .iter()
        .filter_map(|thread| {
            let id = text(thread, "id")?;
            let root = rows.iter().find(|r| r.id == id)?.clone();
            let (room, kind) = sync.store().room_name(&root.rid).map_or((String::new(), None), |(n, k)| (n, Some(k)));
            let people = participants(sync, thread);
            let unread_mentions = int(thread, "unread_mentions");
            let participated = took_part(sync.me(), &root.author_id, thread, kind.as_deref());
            let mentioned = unread_mentions > 0
                || sync.directory.username(sync.me()).is_some_and(|me| names(root.text.as_deref().unwrap_or(""), &me));
            Some(Entry {
                room,
                last_reply: int(thread, "last_reply_at"),
                replies: int(thread, "reply_count"),
                participants: people,
                unread_replies: int(thread, "unread_replies"),
                unread_mentions,
                participated,
                mentioned,
                root,
            })
        })
        .collect();
    Ok(Page { entries, next })
}

/// Counted from the unread threads themselves: each team's own totals count
/// the DMs and groups again.
pub async fn totals(rest: &RestClient) -> Result<Totals, RestError> {
    let mut seen = HashSet::new();
    let mut totals = Totals::default();
    for team in teams(rest).await? {
        let query =
            [("unread", "true".to_owned()), ("threadsOnly", "true".to_owned()), ("per_page", TOTALS_PAGE.to_string())];
        let answer = rest.get(&format!("users/me/teams/{team}/threads"), CallOptions::params(query)).await?;
        for thread in answer.get("threads").and_then(Value::as_array).into_iter().flatten() {
            if text(thread, "id").is_some_and(|id| seen.insert(id.to_owned())) {
                totals.unread_threads += 1;
                totals.unread_mentions += int(thread, "unread_mentions");
            }
        }
    }
    Ok(totals)
}

/// The thread's team: its channel's, any of mine for a DM or a group.
async fn team_of(sync: &MmSync, rid: &str) -> Result<String, RestError> {
    if let Some(team) = sync.team_of(rid) {
        return Ok(team);
    }
    teams(sync.rest()).await?.into_iter().next().ok_or_else(|| RestError::incomplete("users/me/teams: no team"))
}

pub async fn mark_read(sync: &MmSync, rid: &str, root: &str, at: i64) -> Result<(), RestError> {
    let team = team_of(sync, rid).await?;
    sync.rest().put(&format!("users/me/teams/{team}/threads/{root}/read/{at}"), CallOptions::default()).await?;
    Ok(())
}

pub async fn mark_all_read(rest: &RestClient) -> Result<(), RestError> {
    for team in teams(rest).await? {
        rest.put(&format!("users/me/teams/{team}/threads/read"), CallOptions::default()).await?;
    }
    Ok(())
}

pub async fn follow(sync: &MmSync, rid: &str, root: &str, on: bool) -> Result<(), RestError> {
    let team = team_of(sync, rid).await?;
    let path = format!("users/me/teams/{team}/threads/{root}/following");
    if on {
        sync.rest().put(&path, CallOptions::default()).await?;
    } else {
        sync.rest().delete(&path, CallOptions::default()).await?;
    }
    Ok(())
}

/// The server has no such field: a thread I do not follow answers 404.
pub async fn following(sync: &MmSync, rid: &str, root: &str) -> Result<bool, RestError> {
    let team = team_of(sync, rid).await?;
    match sync.rest().get(&format!("users/me/teams/{team}/threads/{root}"), CallOptions::default()).await {
        Ok(_) => Ok(true),
        Err(e) if e.status == 404 => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn thread(id: &str, at: i64) -> Value {
        json!({"id": id, "last_reply_at": at})
    }

    fn ids(threads: &[Value]) -> Vec<&str> {
        threads.iter().filter_map(|t| text(t, "id")).collect()
    }

    fn team(team: &str, threads: Vec<Value>, full: bool) -> TeamPage {
        TeamPage { team: team.to_owned(), before: None, threads, full }
    }

    #[test]
    fn a_replier_or_a_direct_conversation_counts_as_having_taken_part() {
        let thread = json!({"participants": [{"id": "ann"}, {"id": "me"}]});
        assert!(took_part("me", "bob", &thread, Some("c")));
        assert!(!took_part("zoe", "bob", &thread, Some("c")));
        assert!(took_part("zoe", "bob", &thread, Some("d")));
        assert!(took_part("zoe", "zoe", &json!({}), None));
    }

    #[test]
    fn a_team_with_more_to_read_holds_back_what_could_sort_after_it() {
        let (merged, next) = merge(vec![
            team("a", vec![thread("a1", 90), thread("a2", 50)], true),
            team("b", vec![thread("b1", 80), thread("b2", 40), thread("b3", 30)], false),
        ]);
        assert_eq!(ids(&merged), ["a1", "b1", "a2"]);
        let cursor = parse_cursor(&next.unwrap());
        assert_eq!(cursor["a"], "a2");
        assert_eq!(cursor["b"], "b1");
    }

    #[test]
    fn a_conversation_listed_by_every_team_shows_once() {
        let (merged, next) = merge(vec![
            team("a", vec![thread("dm", 90), thread("a1", 50)], false),
            team("b", vec![thread("dm", 90)], false),
        ]);
        assert_eq!(ids(&merged), ["dm", "a1"]);
        assert_eq!(next, None);
    }

    #[test]
    fn a_team_read_to_its_end_leaves_the_cursor() {
        let (_, next) = merge(vec![team("a", vec![thread("a1", 90)], false), team("b", vec![thread("b1", 80)], true)]);
        assert_eq!(parse_cursor(&next.unwrap()).keys().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn a_team_held_back_from_its_start_is_read_again_from_its_start() {
        let (merged, next) =
            merge(vec![team("a", vec![thread("a1", 90)], true), team("b", vec![thread("b1", 10)], false)]);
        assert_eq!(ids(&merged), ["a1"]);
        assert_eq!(parse_cursor(&next.unwrap())["b"], "");
    }

    #[test]
    fn a_team_with_nothing_shown_keeps_its_position() {
        let mut held = team("b", vec![thread("b1", 10)], false);
        held.before = Some("b0".to_owned());
        let (merged, next) = merge(vec![team("a", vec![thread("a1", 90)], true), held]);
        assert_eq!(ids(&merged), ["a1"]);
        assert_eq!(parse_cursor(&next.unwrap())["b"], "b0");
    }
}
