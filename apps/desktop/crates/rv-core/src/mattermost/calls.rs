//! Mattermost's Calls plugin (`com.mattermost.calls`) over REST: whether it runs,
//! whether a channel's calls are on, its ICE servers and who is in a call. The
//! call itself is the voice sidecar's (rv-call `mattermost`).

use rv_provider::rest::{CallOptions, RestClient, RestError};
use serde_json::Value;

const PLUGIN: &str = "plugins/com.mattermost.calls";

fn outside() -> CallOptions {
    CallOptions { outside_api_v1: true, ..Default::default() }
}

/// The plugin's version: an error when it is not installed or not running.
pub async fn version(rest: &RestClient) -> Result<Value, RestError> {
    rest.get(&format!("{PLUGIN}/version"), outside()).await
}

/// Whether a call can be started in this channel (the server's free edition
/// allows direct messages only, and each channel can turn calls off).
pub async fn enabled(rest: &RestClient, channel: &str) -> bool {
    match rest.get(&format!("{PLUGIN}/{channel}"), outside()).await {
        Ok(state) => match state.get("enabled").and_then(Value::as_bool) {
            Some(enabled) => enabled,
            None => default_enabled(rest).await,
        },
        Err(_) => false,
    }
}

async fn default_enabled(rest: &RestClient) -> bool {
    let config = rest.get(&format!("{PLUGIN}/config"), outside()).await.unwrap_or(Value::Null);
    config.get("DefaultEnabled").and_then(Value::as_bool).unwrap_or(false)
}

/// The STUN and TURN servers the plugin hands its clients, TURN credentials included when it needs them.
pub async fn ice_servers(rest: &RestClient) -> Vec<rv_voice_protocol::IceServerSpec> {
    let config = rest.get(&format!("{PLUGIN}/config"), outside()).await.unwrap_or(Value::Null);
    let mut servers = servers_of(config.get("ICEServersConfigs"));
    if config.get("NeedsTURNCredentials").and_then(Value::as_bool) == Some(true)
        && let Ok(turn) = rest.get(&format!("{PLUGIN}/turn-credentials"), outside()).await
    {
        servers.extend(servers_of(Some(&turn)));
    }
    servers
}

fn servers_of(list: Option<&Value>) -> Vec<rv_voice_protocol::IceServerSpec> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let urls: Vec<String> = match s.get("urls") {
                Some(Value::String(url)) => vec![url.clone()],
                Some(Value::Array(urls)) => urls.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                _ => return None,
            };
            Some(rv_voice_protocol::IceServerSpec {
                urls,
                username: text(s, "username"),
                credential: text(s, "credential"),
            })
        })
        .collect()
}

/// Who is in a channel's call: each session id with its user id.
pub async fn sessions(rest: &RestClient, channel: &str) -> Vec<(String, String)> {
    let state = rest.get(&format!("{PLUGIN}/{channel}"), outside()).await.unwrap_or(Value::Null);
    people_of(&state)
}

fn people_of(state: &Value) -> Vec<(String, String)> {
    let sessions = state.pointer("/call/sessions").and_then(Value::as_array);
    sessions
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s.get("session_id")?.as_str()?.to_owned(), s.get("user_id")?.as_str()?.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_plugins_servers_and_people_are_read() {
        let config = json!([{"urls": ["stun:stun.example:3478"]}, {"urls": "turn:t.example", "username": "u", "credential": "c"}, {}]);
        let servers = servers_of(Some(&config));
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].urls, vec!["turn:t.example".to_owned()]);
        assert_eq!((servers[1].username.as_str(), servers[1].credential.as_str()), ("u", "c"));
        let state = json!({"call": {"sessions": [{"session_id": "s1", "user_id": "u1"}, {"session_id": "s2"}]}});
        assert_eq!(people_of(&state), vec![("s1".to_owned(), "u1".to_owned())]);
        assert!(people_of(&json!({"channel_id": "c", "enabled": true})).is_empty());
    }
}
