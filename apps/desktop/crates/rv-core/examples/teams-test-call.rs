//! Live: a full account session places Microsoft's Test Call (it rings no one)
//! through `Session::place_teams_call` and an `rv-voice` sidecar, prints what the
//! voice session does, and hangs up.
//!
//!   secret-tool lookup application me.barrut.RocketVibe kind session account '<key>' \
//!     | RV_VOICE_BIN=path/to/rv-voice RV_VOICE_FAKE_AUDIO=sine cargo run -p rv-core --example teams-test-call
//!
//! The account's secret comes on stdin and is never printed or written.

use std::io::Read;
use std::time::Duration;

use rv_core::session::{Session, SessionInfo};
use rv_core::teams_call::Target;
use rv_core::voice::VoiceController;

#[tokio::main]
async fn main() {
    let mut secret = String::new();
    std::io::stdin().read_to_string(&mut secret).unwrap();
    let info = SessionInfo::from_secret(&serde_json::from_str(&secret).expect("secret json")).expect("an account");
    secret.clear();
    let dir = tempfile::tempdir().unwrap();
    let session = Session::start(info, &dir.path().join("probe.sqlite")).unwrap();
    let voice = VoiceController::new();
    let mut placed = Err("not tried".to_owned());
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        placed = session.place_teams_call(&voice, Target::TestCall, "Test Call").await.map_err(|e| e.message);
        match &placed {
            Err(message) if message.contains("not registered") => continue,
            _ => break,
        }
    }
    println!("placed: {placed:?}");
    let seconds = std::env::var("TEAMS_PROBE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40u64);
    let mut last = String::new();
    for _ in 0..seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let s = voice.snapshot();
        let people: Vec<String> = s
            .participants
            .iter()
            .map(|p| format!("{}{}", p.name.as_deref().unwrap_or("me"), if p.speaking { " speaking" } else { "" }))
            .collect();
        let line = format!("{:?} {people:?} ended {:?}", s.state, s.ended);
        if line != last {
            println!("{line}");
            last = line;
        }
        if s.ended.is_some() {
            return;
        }
    }
    voice.disconnect().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("ended: {:?}", voice.snapshot().ended);
}
