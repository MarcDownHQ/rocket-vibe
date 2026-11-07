mod common;
use common::{FakeHttp, Request, Response, respond};
use rv_core::native::{Identity, NativeSession};
use rv_core::provider::Chat;
use rv_core::thread_inbox::{Filter, Scope};
use rv_protocol::{Room, Snapshot};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../../../docs/protocol/v1.fixture.json")).unwrap()
}

fn me() -> String {
    fixture()["session"]["user"]["id"].as_str().unwrap().to_owned()
}

fn room() -> Room {
    let mut r: Room = serde_json::from_value(fixture()["room"].clone()).unwrap();
    r.read_state = Some(Box::new(
        serde_json::from_value(json!({"room_id": r.id, "membership_version": "grant", "revision": "1", "root_position": "0", "reply_position": "0", "unread_roots": "0", "unread_replies": "0", "mentions": "0", "group_mentions": "0", "favorite": false}))
            .unwrap(),
    ));
    r
}

fn root(id: &str, author: &str, text: &str) -> Value {
    let mut m = fixture()["message"].clone();
    m["id"] = json!(id);
    m["text"] = json!(text);
    m["author"]["id"] = json!(author);
    m["room_id"] = room().id.into();
    m["thread"] = json!({"replies": "2", "last_reply_at": "2026-11-07T10:00:00Z"});
    m
}

fn listed(id: &str, author: &str, text: &str, replier: &str, unread: &str, mentions: &str) -> Value {
    json!({
        "root": root(id, author, text),
        "room_id": room().id,
        "last_reply_position": "40",
        "last_reply_at": "2026-11-07T10:00:00Z",
        "replies": "2",
        "participants": [{"id": replier, "username": "carol", "display_name": "Carol", "avatar_file_id": "file-1"}],
        "following": true,
        "unread": unread,
        "mentions": mentions,
    })
}

fn common_route(req: &Request, thread_inbox: bool) -> Option<Response> {
    let f = fixture();
    Some(match req.path().strip_prefix("/native").unwrap_or(req.path()) {
        "/.well-known/vibez" => {
            let mut d = f["discovery"].clone();
            d["capabilities"]["thread_inbox"] = json!(thread_inbox);
            d["capabilities"]["thread_follow"] = json!(thread_inbox);
            respond(200, &d.to_string())
        }
        "/api/v1/me" => respond(200, &f["session"]["user"].to_string()),
        "/api/v1/sync/changes" => {
            respond(200, &json!({"protocol_version":1,"changes":[],"cursor":"initial","has_more":false}).to_string())
        }
        "/api/v1/sync/ticket" => respond(200, &f["socket_ticket"].to_string()),
        "/api/v1/sync/socket" => Response { websocket: true, ..Default::default() },
        _ => return None,
    })
}

async fn online(server: &FakeHttp) -> Arc<NativeSession> {
    let info = rv_core::session::SessionInfo {
        mattermost: None,
        slack: None,
        teams: None,
        base_url: format!("{}/native", server.url.as_str().trim_end_matches('/')),
        user_id: me(),
        username: "alice".into(),
        auth_token: "fixture-token".into(),
        native: Some(Identity { instance_id: "fixture-instance".into(), data_epoch: "fixture-epoch".into() }),
    };
    let session = NativeSession::start(info, Path::new(":memory:")).unwrap();
    session
        .store
        .snapshot(&Snapshot { protocol_version: 1, rooms: vec![room()], messages: vec![], cursor: "initial".into() })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while session.status().connection != rv_core::session::Connection::Online {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    session
}

#[tokio::test]
async fn the_list_pages_dedupes_and_says_what_touches_me() {
    let mine = me();
    let server = FakeHttp::start(move |req| {
        if let Some(answer) = common_route(req, true) {
            return answer;
        }
        let path = req.path().strip_prefix("/native").unwrap_or(req.path());
        let target = req.target.as_str();
        match path {
            "/api/v1/threads" if target.contains("before=40") => {
                assert!(target.contains("filter=followed"), "{target}");
                respond(
                    200,
                    &json!({"threads": [listed("other", "bob", "plain", "carol", "0", "0")]}).to_string(),
                )
            }
            "/api/v1/threads" if target.contains("filter=unread") => respond(
                200,
                &json!({"threads": [listed("named", "bob", "ping", "carol", "3", "1")]}).to_string(),
            ),
            "/api/v1/threads" => respond(
                200,
                &json!({
                    "threads": [
                        listed("mine", &mine, "my root", "carol", "1", "0"),
                        listed("mine", &mine, "my root", "carol", "1", "0"),
                        listed("replied", "bob", "a root", &mine, "0", "0"),
                        listed("text", "bob", "hello @Alice!", "carol", "0", "0"),
                    ],
                    "next": "40",
                })
                .to_string(),
            ),
            "/api/v1/threads/totals" => {
                respond(200, &json!({"unread_threads": "3", "unread_mentions": "1"}).to_string())
            }
            "/api/v1/threads/read" => {
                assert_eq!(req.method, "POST");
                let body: Value = serde_json::from_str(&req.body).unwrap();
                assert_eq!(body["operation_id"].as_str().unwrap().len(), 32);
                respond(
                    200,
                    &json!({"operation_id": body["operation_id"], "threads": "3", "applied_revision": "9"}).to_string(),
                )
            }
            "/api/v1/messages/mine/follow" => {
                assert_eq!(req.method, "PUT");
                let body: Value = serde_json::from_str(&req.body).unwrap();
                let mut answered = root("mine", "bob", "my root");
                answered["personal_follow"] = json!({"present": body["present"], "revision": "77"});
                respond(200, &answered.to_string())
            }
            other => panic!("unexpected {other}"),
        }
    })
    .await;
    let session = online(&server).await;
    assert!(session.thread_inbox_available() && session.thread_follow_available());
    let chat = Chat::Native(session.clone());
    assert!(chat.thread_inbox_available() && !chat.thread_inbox_polled());

    let page = chat.thread_inbox(Filter::Followed, None).await.unwrap();
    assert_eq!(page.next.as_deref(), Some("40"));
    let ids: Vec<&str> = page.entries.iter().map(|e| e.root.id.as_str()).collect();
    assert_eq!(ids, ["mine", "replied", "text"], "a thread listed twice shows once");
    let [authored, replied, named] = &page.entries[..] else { unreachable!() };
    assert!(authored.participated && !authored.mentioned);
    assert!(replied.participated && !replied.mentioned);
    assert!(!named.participated && named.mentioned, "the root names me");
    assert_eq!(authored.root.rid, room().id);
    assert_eq!(authored.room, room().name);
    assert_eq!(authored.replies, 2);
    assert_eq!(authored.unread_replies, 1);
    assert_eq!(authored.participants[0].photo.as_deref(), Some("file-1"));
    assert_eq!(authored.participants[0].label, "Carol");
    assert!(session.store.messages(&room().id, 10).unwrap().is_empty(), "the list leaves the room's history alone");

    let last = chat.thread_inbox(Filter::Followed, page.next.as_deref()).await.unwrap();
    assert_eq!(last.next, None);
    assert!(Scope::Others.matches(&last.entries[0]));

    let unread = chat.thread_inbox(Filter::Unread, None).await.unwrap();
    assert_eq!((unread.entries[0].unread_replies, unread.entries[0].unread_mentions), (3, 1));
    assert!(unread.entries[0].mentioned, "the server's count of mentions is enough");

    let totals = chat.thread_totals().await.unwrap();
    assert_eq!((totals.unread_threads, totals.unread_mentions), (3, 1));
    chat.mark_all_threads_read().await.unwrap();

    assert!(!session.thread_following("mine").unwrap());
    session.follow_thread("mine", true).await.unwrap();
    assert!(session.thread_following("mine").unwrap());
    session.follow_thread("mine", false).await.unwrap();
    drop(chat);
    common::close_native(session).await;
}

#[tokio::test]
async fn a_server_without_the_capability_is_never_asked() {
    let server = FakeHttp::start(|req| {
        common_route(req, false).unwrap_or_else(|| panic!("unexpected {}", req.target))
    })
    .await;
    let session = online(&server).await;
    assert!(!session.thread_inbox_available() && !session.thread_follow_available());
    assert!(session.thread_inbox(Filter::Followed, None).await.is_err());
    assert!(session.mark_all_threads_read().await.is_err());
    assert!(session.follow_thread("root", true).await.is_err());
    assert_eq!(session.thread_totals().await.unwrap(), Default::default());
    common::close_native(session).await;
}

