//! The ingest API against the test database: batches that are safe to repeat, the ranges at
//! their boundaries, last-writer-wins upserts, ownership and the limit.

// The included modules carry what the binary uses and the tests do not.
#![allow(dead_code)]

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

mod common;

#[path = "../src/accounts.rs"]
mod accounts;
#[path = "../src/app.rs"]
mod app;
#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/errors.rs"]
mod errors;
#[path = "../src/geo.rs"]
mod geo;
#[path = "../src/health.rs"]
mod health;
#[path = "../src/ingest.rs"]
mod ingest;
#[path = "../src/ratelimit.rs"]
mod ratelimit;
#[path = "../src/read.rs"]
mod read;

use common::*;

/// A kept fix at the given time, inside every range.
fn point(at: &str) -> Value {
    json!({
        "recorded_at": at, "tz": "Europe/Warsaw", "lat": 522297700, "lon": 210122200, "h_acc_m": 8,
        "speed_cms": 140, "course_deg": 90, "activity": 2, "activity_conf": 2, "source": "phone",
        "battery_pct": 80, "battery_state": 1, "coarse": false
    })
}

/// `count` times a second apart from 2026-10-12 10:00 UTC, offset by `shift` seconds.
fn times(count: usize, shift: i64) -> Vec<String> {
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-12T10:00:00Z").unwrap();
    (0..count)
        .map(|i| {
            (start + chrono::Duration::seconds(shift + i as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .collect()
}

fn batch(batch_id: Uuid, device_id: Uuid, points: Vec<Value>) -> Value {
    json!({"batch_id": batch_id, "device_id": device_id, "points": points})
}

async fn post_batch(router: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
    call(router, "POST", "/api/v1/points/batches", body, Some(token)).await
}

fn stop(id: Uuid, updated_at: &str) -> Value {
    json!({
        "id": id, "kind": "stop", "start_at": "2026-10-12T10:00:00Z", "end_at": "2026-10-12T10:20:00Z",
        "tz": "Europe/Warsaw", "mode_detected": "stationary", "mode_conf": "high", "mode_type": "other",
        "distance_m": 0, "point_count": 12, "center_lat": 522297700, "center_lon": 210122200,
        "source": "phone", "computed_by": "phone", "algorithm_version": 1, "coarse": false,
        "updated_at": updated_at
    })
}

#[tokio::test]
async fn a_batch_sent_twice_is_stored_once_and_answers_the_same_counts() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let batch_id = Uuid::now_v7();
    let points: Vec<Value> = times(1000, 0).iter().map(|t| point(t)).collect();

    let (status, body) =
        post_batch(&router, &me.token, batch(batch_id, me.device_id, points)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, json!({"accepted": 1000, "duplicates": 0}));

    let other: Vec<Value> = times(1000, 5000).iter().map(|t| point(t)).collect();
    let (status, body) = post_batch(&router, &me.token, batch(batch_id, me.device_id, other)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"accepted": 1000, "duplicates": 0}));

    let stored: i64 = sqlx::query_scalar("select count(*) from points where device_id = $1")
        .bind(me.device_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1000);
    let record: (Uuid, i32, i32, i32) = sqlx::query_as(
        "select device_id, point_count, accepted, duplicates from upload_batches where id = $1",
    )
    .bind(batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(record, (me.device_id, 1000, 1000, 0));

    // Ten points already there and five new ones, in a new batch.
    let mut mixed: Vec<Value> = times(10, 0).iter().map(|t| point(t)).collect();
    mixed.extend(times(5, 2000).iter().map(|t| point(t)));
    let (status, body) = post_batch(
        &router,
        &me.token,
        batch(Uuid::now_v7(), me.device_id, mixed),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, json!({"accepted": 5, "duplicates": 10}));

    // Two points at the same second in one batch: the first counts.
    let twins = vec![point(&times(1, 3000)[0]), point(&times(1, 3000)[0])];
    let (status, body) = post_batch(
        &router,
        &me.token,
        batch(Uuid::now_v7(), me.device_id, twins),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body, json!({"accepted": 1, "duplicates": 1}));
}

#[tokio::test]
async fn a_batch_id_or_a_device_of_another_account_is_refused() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let other = signed_in(&router, &pool).await;
    let batch_id = Uuid::now_v7();
    let (status, _) = post_batch(
        &router,
        &me.token,
        batch(batch_id, me.device_id, vec![point(&times(1, 0)[0])]),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = post_batch(
        &router,
        &other.token,
        batch(batch_id, other.device_id, vec![point(&times(1, 0)[0])]),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "batch_id_taken");

    let (status, body) = post_batch(
        &router,
        &other.token,
        batch(Uuid::now_v7(), me.device_id, vec![point(&times(1, 10)[0])]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let stored: i64 = sqlx::query_scalar("select count(*) from points where device_id = $1")
        .bind(me.device_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);

    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/points/batches",
        batch(Uuid::now_v7(), me.device_id, vec![point(&times(1, 20)[0])]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn a_body_over_five_megabytes_or_not_json_is_refused() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let big = vec![b' '; ingest::BODY_LIMIT_BYTES + 1];
    let (status, _, body) = call_raw(
        &router,
        "POST",
        "/api/v1/points/batches",
        big,
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "payload_too_large");

    let (status, _, body) = call_raw(
        &router,
        "POST",
        "/api/v1/points/batches",
        b"{not json".to_vec(),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_json");

    let (status, body) = post_batch(
        &router,
        &me.token,
        batch(Uuid::now_v7(), me.device_id, vec![]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "validation");
}

#[tokio::test]
async fn every_point_range_is_checked_at_its_boundary() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let soon = (chrono::Utc::now() + chrono::Duration::minutes(4))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let too_soon = (chrono::Utc::now() + chrono::Duration::minutes(6))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let cases: Vec<(&str, Value, Value)> = vec![
        ("lat", json!(900000000), json!(900000001)),
        ("lat", json!(-900000000), json!(-900000001)),
        ("lon", json!(1800000000), json!(1800000001)),
        ("lon", json!(-1800000000), json!(-1800000001)),
        ("h_acc_m", json!(32767), json!(32768)),
        ("h_acc_m", json!(0), json!(-1)),
        ("v_acc_m", json!(32767), json!(32768)),
        ("v_acc_m", json!(null), json!(-1)),
        ("alt_m", json!(-1000), json!(-1001)),
        ("alt_m", json!(32767), json!(32768)),
        ("speed_cms", json!(-1), json!(-2)),
        ("speed_cms", json!(32767), json!(32768)),
        ("course_deg", json!(359), json!(360)),
        ("course_deg", json!(-1), json!(-2)),
        ("activity", json!(5), json!(6)),
        ("activity", json!(0), json!(-1)),
        ("activity_conf", json!(2), json!(3)),
        ("battery_pct", json!(100), json!(101)),
        ("battery_pct", json!(0), json!(-1)),
        ("battery_state", json!(3), json!(4)),
        ("source", json!("visit"), json!("import:gpx")),
        ("source", json!("significant_change"), json!("watch")),
        ("tz", json!("Europe/Warsaw"), json!("Mars/Olympus")),
        ("recorded_at", json!(soon), json!(too_soon)),
        (
            "recorded_at",
            json!("2000-01-01T00:00:00Z"),
            json!("1999-12-31T23:59:59Z"),
        ),
        (
            "recorded_at",
            json!("2026-10-12T12:00:00+02:00"),
            json!("2026-10-12 12:00:00"),
        ),
        ("coarse", json!(true), json!(null)),
        ("import_batch_id", json!(null), json!(Uuid::now_v7())),
    ];
    for (index, (field, inside, outside)) in cases.iter().enumerate() {
        let at = times(1, 100 + index as i64)[0].clone();
        let mut good = point(&at);
        good[*field] = inside.clone();
        let (status, body) = post_batch(
            &router,
            &me.token,
            batch(Uuid::now_v7(), me.device_id, vec![good]),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{field} = {inside}: {body}");

        let mut bad = point(&at);
        bad[*field] = outside.clone();
        let (status, body) = post_batch(
            &router,
            &me.token,
            batch(
                Uuid::now_v7(),
                me.device_id,
                vec![point(&times(1, 50)[0]), bad],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{field} = {outside}: {body}"
        );
        assert_eq!(
            body["error"]["code"], "invalid_point",
            "{field} = {outside}"
        );
        assert_eq!(body["error"]["index"], 1, "{field} = {outside}");
        assert_eq!(body["error"]["field"], *field, "{field} = {outside}");
    }
    // The good point of every refused batch was not stored: one bad point rejects the batch.
    let stored: i64 = sqlx::query_scalar(
        "select count(*) from points where device_id = $1 and recorded_at = '2026-10-12T10:00:50Z'",
    )
    .bind(me.device_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn segments_apply_the_newer_row_and_ignore_the_rest() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let other = signed_in(&router, &pool).await;
    let id = Uuid::now_v7();
    let put = |token: String, rows: Vec<Value>| {
        let router = router.clone();
        async move {
            call(
                &router,
                "PUT",
                "/api/v1/segments",
                json!({"segments": rows}),
                Some(&token),
            )
            .await
        }
    };

    let (status, body) = put(me.token.clone(), vec![stop(id, "2026-10-12T11:00:00Z")]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let synced_first: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("select synced_at from segments where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();

    // The same row again, an equal and an older one: all ignored.
    let (_, body) = put(me.token.clone(), vec![stop(id, "2026-10-12T11:00:00Z")]).await;
    assert_eq!(body, json!({"applied": 0, "ignored": 1}));
    let mut older = stop(id, "2026-10-12T10:59:59Z");
    older["point_count"] = json!(99);
    let (_, body) = put(me.token.clone(), vec![older]).await;
    assert_eq!(body, json!({"applied": 0, "ignored": 1}));
    let count: i32 = sqlx::query_scalar("select point_count from segments where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 12);

    // A soft delete with a newer clock is applied and never removes the row.
    let mut deleted = stop(id, "2026-10-12T11:05:00Z");
    deleted["deleted_at"] = json!("2026-10-12T11:05:00Z");
    let (_, body) = put(me.token.clone(), vec![deleted]).await;
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let (deleted_at, synced_second): (
        Option<chrono::DateTime<chrono::Utc>>,
        chrono::DateTime<chrono::Utc>,
    ) = sqlx::query_as("select deleted_at, synced_at from segments where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(deleted_at.is_some());
    assert!(synced_second >= synced_first);

    // A newer row without deleted_at undeletes it.
    let (_, body) = put(me.token.clone(), vec![stop(id, "2026-10-12T11:10:00Z")]).await;
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("select deleted_at from segments where id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted_at.is_none());

    // Another account cannot touch the row, and nothing of its request is stored.
    let theirs = Uuid::now_v7();
    let (status, body) = put(
        other.token.clone(),
        vec![
            stop(theirs, "2026-10-12T12:00:00Z"),
            stop(id, "2026-10-12T12:00:00Z"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let exists: bool = sqlx::query_scalar("select exists(select 1 from segments where id = $1)")
        .bind(theirs)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!exists);
    let owner: Uuid = sqlx::query_scalar("select user_id from segments where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, me.user_id);

    // The rows are validated, naming the first failure.
    let mut trip = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    trip["kind"] = json!("trip");
    let (status, body) = put(me.token.clone(), vec![trip]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["field"], "center_lat");
    let mut server = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    server["computed_by"] = json!("server");
    let (_, body) = put(
        me.token.clone(),
        vec![stop(id, "2026-10-12T12:00:00Z"), server],
    )
    .await;
    assert_eq!(body["error"]["index"], 1);
    assert_eq!(body["error"]["field"], "computed_by");
    let mut jetpack = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    jetpack["mode_type"] = json!("jetpack");
    let (_, body) = put(me.token.clone(), vec![jetpack]).await;
    assert_eq!(body["error"]["field"], "mode_type");
    let mut backwards = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    backwards["end_at"] = json!("2026-10-12T09:00:00Z");
    let (_, body) = put(me.token.clone(), vec![backwards]).await;
    assert_eq!(body["error"]["field"], "end_at");
    let mut foreign_device = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    foreign_device["device_id"] = json!(other.device_id);
    let (status, _) = put(me.token.clone(), vec![foreign_device]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn corrections_places_and_markers_upsert_by_id() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let other = signed_in(&router, &pool).await;

    let place_id = Uuid::now_v7();
    let place = json!({
        "id": place_id, "name": "Home", "lat": 522297700, "lon": 210122200, "radius_m": 100,
        "created_at": "2026-10-12T09:00:00Z", "updated_at": "2026-10-12T09:00:00Z"
    });
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/places",
        json!({"places": [place]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/places",
        json!({"places": [place]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body, json!({"applied": 0, "ignored": 1}));
    let mut long_name = place.clone();
    long_name["id"] = json!(Uuid::now_v7());
    long_name["name"] = json!("x".repeat(201));
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/places",
        json!({"places": [long_name]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["field"], "name");
    let mut tiny = place.clone();
    tiny["id"] = json!(Uuid::now_v7());
    tiny["radius_m"] = json!(9);
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/places",
        json!({"places": [tiny]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body["error"]["field"], "radius_m");

    // A segment may point at the account's own place, not at another account's.
    let mut at_home = stop(Uuid::now_v7(), "2026-10-12T12:00:00Z");
    at_home["place_id"] = json!(place_id);
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/segments",
        json!({"segments": [at_home.clone()]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    at_home["id"] = json!(Uuid::now_v7());
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/segments",
        json!({"segments": [at_home]}),
        Some(&other.token),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["field"], "place_id");

    let correction = json!({
        "id": Uuid::now_v7(), "kind": "retype", "start_at": "2026-10-12T10:00:00Z",
        "end_at": "2026-10-12T10:20:00Z", "payload": {"mode_type": "bike"},
        "updated_at": "2026-10-12T10:30:00Z"
    });
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/corrections",
        json!({"corrections": [correction]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let mut listed = correction.clone();
    listed["id"] = json!(Uuid::now_v7());
    listed["payload"] = json!([1, 2]);
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/corrections",
        json!({"corrections": [listed]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body["error"]["field"], "payload");
    let mut odd_kind = correction.clone();
    odd_kind["id"] = json!(Uuid::now_v7());
    odd_kind["kind"] = json!("rewrite");
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/corrections",
        json!({"corrections": [odd_kind]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body["error"]["field"], "kind");

    let marker = json!({
        "id": Uuid::now_v7(), "recorded_at": "2026-10-12T10:05:00Z", "tz": "Europe/Warsaw",
        "lat": 522297700, "lon": 210122200, "note": "Found the keys", "updated_at": "2026-10-12T10:05:00Z"
    });
    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/markers",
        json!({"markers": [marker]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"applied": 1, "ignored": 0}));
    let mut essay = marker.clone();
    essay["id"] = json!(Uuid::now_v7());
    essay["note"] = json!("n".repeat(2001));
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/markers",
        json!({"markers": [essay]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body["error"]["field"], "note");
    let mut long_ref = marker.clone();
    long_ref["id"] = json!(Uuid::now_v7());
    long_ref["photo_ref"] = json!("p".repeat(513));
    let (_, body) = call(
        &router,
        "PUT",
        "/api/v1/markers",
        json!({"markers": [long_ref]}),
        Some(&me.token),
    )
    .await;
    assert_eq!(body["error"]["field"], "photo_ref");

    let (status, body) = call(
        &router,
        "PUT",
        "/api/v1/markers",
        json!({"markers": []}),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn the_one_hundred_twenty_first_request_in_a_minute_is_refused() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let place = json!({
        "id": Uuid::now_v7(), "name": "Office", "lat": 522041000, "lon": 210482000,
        "updated_at": "2026-10-12T09:00:00Z"
    });
    for _ in 0..ingest::REQUESTS_PER_MINUTE {
        let (status, body) = call(
            &router,
            "PUT",
            "/api/v1/places",
            json!({"places": [place]}),
            Some(&me.token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, headers, body) = call_raw(
        &router,
        "PUT",
        "/api/v1/places",
        json!({"places": [place]}).to_string().into(),
        Some(&me.token),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["code"], "rate_limited");
    assert!(headers.get("retry-after").is_some());
}
