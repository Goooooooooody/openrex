//! Checks that the activity tracker reads what a running Rex server sends:
//! the block events of a program's status, title, progress, bell, and exit,
//! and the results of `title` and `program_status`.
//!
//! It needs a server on the local socket (or `REX_SERVER`), so it is ignored
//! by default:
//!
//!   cargo test -p openrex --test live_activity -- --ignored
//!
//! It makes its own session and destroys it afterwards.

use std::time::{Duration, Instant};

use futures::FutureExt;
use openrex::activity::{Level, Tracker};
use openrex_proto::Endpoint;
use openrex_proto::control::{self, Hello};
use serde_json::json;

#[test]
#[ignore = "needs a running Rex server"]
fn the_tracker_follows_a_live_terminal() {
    smol::block_on(async {
        let endpoint = Endpoint::from_env().expect("endpoint");
        let hello = Hello { name: "openrex-test".into(), version: "0".into(), ..Default::default() };
        let conn = control::connect(&endpoint, hello).await.expect("connect to the Rex server");
        let control = conn.control.clone();
        let events = conn.events.clone();
        smol::spawn(async move {
            conn.driver.await.ok();
        })
        .detach();
        let created = control
            .call(
                "session.create",
                json!({
                    "label": format!("openrex-test-activity-{}", std::process::id()),
                    "initial_windows": [{"layout": {"block": {
                        "flavor": "com.superlogical.terminal.shell",
                        "options": {"shell": "none", "command": ["sh"]},
                    }}}],
                }),
            )
            .await
            .expect("session.create");
        let session_id = created["session_id"].as_str().unwrap().to_string();
        let block_id = created["initial_windows"][0]["block_ids"][0].as_str().unwrap().to_string();
        control.call("session.attach", json!({"session_id": session_id})).await.expect("session.attach");

        // A program blocks on a question, names the terminal, reports
        // progress, and rings the bell.
        let script = "printf '\\033]7501;state=blocked:kind=question:app=live:msg=%s\\033\\\\' $(printf 'Which one?' | base64); \
                      printf '\\033]2;live title\\007'; printf '\\033]9;4;1;30\\007'; printf '\\007'\n";
        control
            .block_call("com.superlogical.terminal.write", &session_id, &block_id, json!({"data": script}))
            .await
            .expect("write");

        let mut tracker = Tracker::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        let complete = |t: &Tracker| {
            let b = t.get(&block_id);
            t.title(&block_id) == Some("live title") && b.is_some_and(|b| b.bell && b.progress.is_some() && !b.records.is_empty())
        };
        while !complete(&tracker) && Instant::now() < deadline {
            let next = futures::select! {
                ev = events.recv().fuse() => ev.ok(),
                _ = smol::Timer::after(Duration::from_millis(200)).fuse() => None,
            };
            let Some(ev) = next else { continue };
            if ev.name != "session_event" || ev.payload["event"]["type"] != "block_event" {
                continue;
            }
            let sid = ev.payload["session_id"].as_str().unwrap_or("");
            let p = &ev.payload["event"]["payload"];
            tracker.on_event(sid, p["block_id"].as_str().unwrap_or(""), p["name"].as_str().unwrap_or(""), &p["payload"]);
        }
        assert!(complete(&tracker), "events never arrived: {:?}", tracker.get(&block_id));
        let entries = tracker.block_entries(&block_id);
        assert_eq!((entries[0].level, entries[0].headline.as_str(), entries[0].message.as_str()), (Level::Blocked, "live", "Which one?"));
        assert_eq!(tracker.session_summary(&session_id).unwrap().level, Level::Blocked);

        // A client that wasn't listening learns the same from the methods.
        let mut fresh = Tracker::default();
        let title = control.block_call("com.superlogical.terminal.title", &session_id, &block_id, json!({})).await.expect("title");
        fresh.fetched_title(&session_id, &block_id, title["title"].as_str().unwrap_or(""));
        let status = control.block_call("com.superlogical.terminal.program_status", &session_id, &block_id, json!({})).await.expect("program_status");
        fresh.fetched_status(&session_id, &block_id, &status);
        assert_eq!(fresh.title(&block_id), Some("live title"));
        assert_eq!(fresh.summary([block_id.as_str()]).unwrap().level, Level::Blocked);

        // The program exits with a failure.
        control
            .block_call("com.superlogical.terminal.write", &session_id, &block_id, json!({"data": "exit 4\n"}))
            .await
            .expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while tracker.get(&block_id).and_then(|b| b.exited).is_none() && Instant::now() < deadline {
            let next = futures::select! {
                ev = events.recv().fuse() => ev.ok(),
                _ = smol::Timer::after(Duration::from_millis(200)).fuse() => None,
            };
            let Some(ev) = next else { continue };
            if ev.name == "session_event" && ev.payload["event"]["type"] == "block_event" {
                let p = &ev.payload["event"]["payload"];
                tracker.on_event(&session_id, p["block_id"].as_str().unwrap_or(""), p["name"].as_str().unwrap_or(""), &p["payload"]);
            }
        }
        control.call("session.destroy", json!({"session_id": session_id})).await.ok();
        control.close();
        assert_eq!(tracker.get(&block_id).and_then(|b| b.exited), Some(4));
    });
}
