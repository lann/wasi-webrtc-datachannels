//! Integration tests for `conformance-signalingd` over real HTTP.
//!
//! Each test spawns a server on an ephemeral localhost port and drives it with
//! a `reqwest` client, exercising the protocol in
//! `conformance/signaling/PROTOCOL.md`: publish/fetch ordering, long-poll
//! wakeup, sequence idempotence, done-markers, room TTL expiry, size caps, and
//! concurrent rooms.

use std::net::SocketAddr;
use std::time::Duration;

use conformance_signalingd::state::Limits;
use conformance_signalingd::{spawn, Config, RunningServer};
use reqwest::StatusCode;

/// Spawn a server with the given config on an ephemeral localhost port.
async fn server_with(config: Config) -> RunningServer {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    spawn(addr, config).await.expect("spawn signaling server")
}

/// Spawn a server with default config (but a snappy long-poll for tests).
async fn server() -> RunningServer {
    server_with(Config {
        long_poll: Duration::from_millis(500),
        ..Config::default()
    })
    .await
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn publish(c: &reqwest::Client, base: &str, room: &str, role: &str, body: &[u8]) -> u64 {
    let resp = c
        .post(format!("{base}/rooms/{room}/{role}"))
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "publish should succeed");
    let json: serde_json::Value = resp.json().await.unwrap();
    json["seq"].as_u64().unwrap()
}

#[tokio::test]
async fn healthz_reports_ready() {
    let s = server().await;
    let base = s.base_url();
    let resp = client()
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "ok");
    s.shutdown().await;
}

#[tokio::test]
async fn publish_and_fetch_preserve_order() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    assert_eq!(publish(&c, &base, "r", "offerer", b"a").await, 0);
    assert_eq!(publish(&c, &base, "r", "offerer", b"b").await, 1);
    assert_eq!(publish(&c, &base, "r", "offerer", b"c").await, 2);

    for (seq, expected) in [(0, "a"), (1, "b"), (2, "c")] {
        let resp = c
            .get(format!("{base}/rooms/r/offerer?seq={seq}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["x-seq"], seq.to_string().as_str());
        assert_eq!(resp.bytes().await.unwrap().as_ref(), expected.as_bytes());
    }
    s.shutdown().await;
}

#[tokio::test]
async fn fetch_is_idempotent_across_refetches() {
    let s = server().await;
    let base = s.base_url();
    let c = client();
    publish(&c, &base, "r", "answerer", b"payload").await;

    for _ in 0..3 {
        let resp = c
            .get(format!("{base}/rooms/r/answerer?seq=0"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"payload");
    }
    s.shutdown().await;
}

#[tokio::test]
async fn long_poll_wakes_on_publish() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    // Start a consumer that blocks on a not-yet-published seq (generous wait).
    let base2 = base.clone();
    let c2 = c.clone();
    let consumer = tokio::spawn(async move {
        let resp = c2
            .get(format!("{base2}/rooms/r/offerer?seq=0&wait=5000"))
            .send()
            .await
            .unwrap();
        (resp.status(), resp.bytes().await.unwrap())
    });

    // Give the consumer time to arrive and block, then publish.
    tokio::time::sleep(Duration::from_millis(150)).await;
    publish(&c, &base, "r", "offerer", b"late").await;

    let (status, body) = consumer.await.unwrap();
    assert_eq!(status, StatusCode::OK, "long-poll should wake on publish");
    assert_eq!(body.as_ref(), b"late");
    s.shutdown().await;
}

#[tokio::test]
async fn short_poll_returns_304_when_absent() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    // wait=0 => immediate non-blocking peek; blob 0 absent => 304 (retry).
    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=0&wait=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()["x-seq"], "0");
    s.shutdown().await;
}

#[tokio::test]
async fn done_marker_yields_no_content_past_end() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    publish(&c, &base, "r", "offerer", b"only").await;
    let resp = c
        .post(format!("{base}/rooms/r/offerer/done"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["len"].as_u64().unwrap(), 1);

    // seq 0 still returns its blob.
    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=0&wait=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // seq 1 (past end, done) => 204 with X-Done.
    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=1&wait=2000"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(resp.headers()["x-done"], "true");
    s.shutdown().await;
}

#[tokio::test]
async fn publish_after_done_conflicts() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    c.post(format!("{base}/rooms/r/offerer/done"))
        .send()
        .await
        .unwrap();
    let resp = c
        .post(format!("{base}/rooms/r/offerer"))
        .body(b"nope".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "done");
    s.shutdown().await;
}

#[tokio::test]
async fn long_poll_wakes_on_done() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    let base2 = base.clone();
    let c2 = c.clone();
    let consumer = tokio::spawn(async move {
        c2.get(format!("{base2}/rooms/r/offerer?seq=0&wait=5000"))
            .send()
            .await
            .unwrap()
            .status()
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    c.post(format!("{base}/rooms/r/offerer/done"))
        .send()
        .await
        .unwrap();

    assert_eq!(consumer.await.unwrap(), StatusCode::NO_CONTENT);
    s.shutdown().await;
}

#[tokio::test]
async fn size_cap_rejects_oversized_publish() {
    let s = server_with(Config {
        long_poll: Duration::from_millis(500),
        max_blob_bytes: 16,
        ..Config::default()
    })
    .await;
    let base = s.base_url();
    let c = client();

    let resp = c
        .post(format!("{base}/rooms/r/offerer"))
        .body(vec![0u8; 17])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["error"], "too-large");
    assert_eq!(json["limit"].as_u64().unwrap(), 16);

    // At the limit is accepted.
    let resp = c
        .post(format!("{base}/rooms/r/offerer"))
        .body(vec![0u8; 16])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    s.shutdown().await;
}

#[tokio::test]
async fn concurrent_rooms_are_isolated() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    publish(&c, &base, "room-a", "offerer", b"aaa").await;
    publish(&c, &base, "room-b", "offerer", b"bbb").await;

    let a = c
        .get(format!("{base}/rooms/room-a/offerer?seq=0"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let b = c
        .get(format!("{base}/rooms/room-b/offerer?seq=0"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(a.as_ref(), b"aaa");
    assert_eq!(b.as_ref(), b"bbb");
    s.shutdown().await;
}

#[tokio::test]
async fn two_roles_cross_fetch() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    publish(&c, &base, "r", "offerer", b"offer").await;
    publish(&c, &base, "r", "answerer", b"answer").await;

    // Each side fetches the *peer's* mailbox.
    let ans = c
        .get(format!("{base}/rooms/r/answerer?seq=0"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let off = c
        .get(format!("{base}/rooms/r/offerer?seq=0"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(ans.as_ref(), b"answer");
    assert_eq!(off.as_ref(), b"offer");
    s.shutdown().await;
}

#[tokio::test]
async fn room_ttl_evicts_idle_rooms() {
    // Short TTL + frequent sweeps so eviction is observable quickly.
    let s = server_with(Config {
        long_poll: Duration::from_millis(200),
        limits: Limits {
            room_ttl: Duration::from_millis(200),
            ..Limits::default()
        },
        eviction_interval: Duration::from_millis(100),
        ..Config::default()
    })
    .await;
    let base = s.base_url();
    let c = client();

    publish(&c, &base, "r", "offerer", b"x").await;

    // Wait past TTL + a sweep tick; the idle room should be evicted, so a fresh
    // fetch of seq 0 finds an empty mailbox (304, not the old blob).
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=0&wait=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_MODIFIED,
        "evicted room should behave as empty"
    );
    s.shutdown().await;
}

#[tokio::test]
async fn delete_room_removes_state() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    publish(&c, &base, "r", "offerer", b"x").await;
    let resp = c.delete(format!("{base}/rooms/r")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], true);

    // Recreated empty on next fetch.
    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=0&wait=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    // Deleting a nonexistent room is idempotent.
    let resp = c.delete(format!("{base}/rooms/nope")).send().await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["deleted"], false);
    s.shutdown().await;
}

#[tokio::test]
async fn bad_seq_and_role_are_rejected() {
    let s = server().await;
    let base = s.base_url();
    let c = client();

    let resp = c
        .get(format!("{base}/rooms/r/offerer?seq=notanumber"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"],
        "bad-seq"
    );

    let resp = c
        .get(format!("{base}/rooms/r/spectator?seq=0"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["error"],
        "bad-role"
    );
    s.shutdown().await;
}
