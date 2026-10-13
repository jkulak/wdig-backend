//! The read API against the test database: the days index with its midnight rule, a day's
//! segments and simplified points, complete and stable paging of points, and the changes
//! stream by the server's clock.

// The included modules carry what the binary uses and the tests do not.
#![allow(dead_code)]

use std::collections::HashSet;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
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

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn rfc(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A point `north_m` metres north of home at `t`, with the given accuracy.
fn point(t: DateTime<Utc>, north_m: i64, h_acc_m: i64, coarse: bool) -> Value {
    json!({
        "recorded_at": rfc(t), "tz": "Europe/Warsaw", "lat": 522297700 + north_m * 90,
        "lon": 210122200, "h_acc_m": h_acc_m, "activity": 2, "activity_conf": 2, "coarse": coarse
    })
}

async fn upload(router: &axum::Router, account: &Account, points: Vec<Value>) {
    let (status, body) = call(
        router,
        "POST",
        "/api/v1/points/batches",
        json!({"batch_id": Uuid::now_v7(), "device_id": account.device_id, "points": points}),
        Some(&account.token),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn put(router: &axum::Router, account: &Account, path: &str, key: &str, rows: Vec<Value>) {
    let (status, body) = call(
        router,
        "PUT",
        path,
        json!({key: rows}),
        Some(&account.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn trip(start: &str, end: &str, distance_m: i64) -> Value {
    json!({
        "id": Uuid::now_v7(), "kind": "trip", "start_at": start, "end_at": end, "tz": "Europe/Warsaw",
        "mode_detected": "walking", "mode_conf": "high", "mode_type": "walk", "distance_m": distance_m,
        "point_count": 10, "source": "phone", "computed_by": "phone", "algorithm_version": 1,
        "coarse": false, "updated_at": end
    })
}

fn stop(start: &str, end: &str) -> Value {
    json!({
        "id": Uuid::now_v7(), "kind": "stop", "start_at": start, "end_at": end, "tz": "Europe/Warsaw",
        "mode_detected": "stationary", "mode_conf": "high", "mode_type": "other", "distance_m": 0,
        "point_count": 10, "center_lat": 522297700, "center_lon": 210122200, "source": "phone",
        "computed_by": "phone", "algorithm_version": 1, "coarse": false, "updated_at": end
    })
}

async fn get(router: &axum::Router, account: &Account, path: &str) -> (StatusCode, Value) {
    call(router, "GET", path, Value::Null, Some(&account.token)).await
}

#[tokio::test]
async fn the_days_index_counts_each_local_day_and_splits_a_midnight_trip() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let other = signed_in(&router, &pool).await;
    let noon = at("2026-10-12T10:00:00Z");
    let mut points: Vec<Value> = (0..100)
        .map(|i| point(noon + Duration::seconds(i), i, 8, false))
        .collect();
    let next = at("2026-10-13T08:00:00Z");
    points.extend((0..50).map(|i| point(next + Duration::seconds(i), i, 8, i == 7)));
    upload(&router, &me, points).await;
    put(
        &router,
        &me,
        "/api/v1/segments",
        "segments",
        vec![
            stop("2026-10-12T10:00:00Z", "2026-10-12T10:20:00Z"),
            // 23:20 to 00:20 Warsaw time (UTC+2): two thirds of the distance before midnight.
            trip("2026-10-12T21:20:00Z", "2026-10-12T22:20:00Z", 1000),
        ],
    )
    .await;

    let (status, body) = get(&router, &me, "/api/v1/days?from=2026-10-12&to=2026-10-13").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!([
            {"date": "2026-10-12", "point_count": 100, "trip_count": 1, "stop_count": 1, "distance_m": 667, "coarse": false},
            {"date": "2026-10-13", "point_count": 50, "trip_count": 1, "stop_count": 0, "distance_m": 333, "coarse": true},
        ])
    );
    let (_, body) = get(&router, &me, "/api/v1/days?from=2026-10-13&to=2026-10-13").await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (_, body) = get(
        &router,
        &other,
        "/api/v1/days?from=2026-10-12&to=2026-10-13",
    )
    .await;
    assert_eq!(body, json!([]));

    for (path, field) in [
        ("/api/v1/days?from=2026-10-13&to=2026-10-12", "to"),
        ("/api/v1/days?from=2026-10-12", "to"),
        ("/api/v1/days?from=12.10.2026&to=2026-10-13", "from"),
        ("/api/v1/days?from=2025-01-01&to=2026-10-13", "to"),
    ] {
        let (status, body) = get(&router, &me, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"]["code"], "validation");
        assert_eq!(body["error"]["field"], field, "{path}");
    }
    let (status, _) = call(
        &router,
        "GET",
        "/api/v1/days?from=2026-10-12&to=2026-10-13",
        Value::Null,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_day_comes_with_its_segments_whole_and_its_points_simplified_by_group() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let start = at("2026-10-12T12:00:00Z");
    // Ten points before the trip, two hundred inside it, all on one straight line north,
    // and twenty wild ones the simplifier never sees.
    let mut points: Vec<Value> = (0..10)
        .map(|i| point(start - Duration::seconds(600 - i), i, 8, false))
        .collect();
    points.extend((0..200).map(|i| point(start + Duration::seconds(i), 100 + i * 10, 8, false)));
    points.extend((0..20).map(|i| {
        point(
            start + Duration::seconds(i * 7),
            5_000 + i * 300,
            150,
            false,
        )
    }));
    upload(&router, &me, points).await;
    let the_trip = trip("2026-10-12T12:00:00Z", "2026-10-12T12:03:19Z", 1990);
    let over_midnight = trip("2026-10-12T21:40:00Z", "2026-10-12T22:20:00Z", 1000);
    put(
        &router,
        &me,
        "/api/v1/segments",
        "segments",
        vec![the_trip.clone(), over_midnight.clone()],
    )
    .await;

    let (status, body) = get(&router, &me, "/api/v1/days/2026-10-12").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["date"], "2026-10-12");
    assert_eq!(body["computed_by"], "phone");
    let segments = body["segments"].as_array().unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0]["id"], the_trip["id"]);
    assert_eq!(segments[0]["end_at"], "2026-10-12T12:03:19Z");
    let points = body["points"].as_array().unwrap();
    // The ten before the trip reduce to their ends, as do the two hundred inside it.
    assert_eq!(points.len(), 4, "{points:?}");
    assert_eq!(points[0]["segment_id"], Value::Null);
    assert_eq!(points[1]["segment_id"], Value::Null);
    assert_eq!(points[2]["segment_id"], the_trip["id"]);
    assert_eq!(points[3]["segment_id"], the_trip["id"]);
    assert_eq!(points[2]["recorded_at"], "2026-10-12T12:00:00Z");
    assert_eq!(points[3]["recorded_at"], "2026-10-12T12:03:19Z");
    assert!(points.iter().all(|p| p["h_acc_m"].as_i64().unwrap() <= 100));
    assert_eq!(
        points[0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<HashSet<_>>(),
        [
            "recorded_at",
            "tz",
            "lat",
            "lon",
            "h_acc_m",
            "speed_cms",
            "activity",
            "coarse",
            "segment_id"
        ]
        .iter()
        .map(|k| k.to_string())
        .collect()
    );

    // The trip over midnight is on the next day too, whole.
    let (_, body) = get(&router, &me, "/api/v1/days/2026-10-13").await;
    assert_eq!(body["segments"][0]["id"], over_midnight["id"]);
    assert_eq!(body["segments"][0]["start_at"], "2026-10-12T21:40:00Z");
    assert_eq!(body["points"], json!([]));

    let (_, body) = get(&router, &me, "/api/v1/days/2026-10-12?computed_by=server").await;
    assert_eq!(body["segments"], json!([]));
    let (_, body) = get(&router, &me, "/api/v1/days/2026-10-11").await;
    assert_eq!(body["segments"], json!([]));
    assert_eq!(body["points"], json!([]));
    for (path, field) in [
        ("/api/v1/days/2026-10-12?computed_by=watch", "computed_by"),
        ("/api/v1/days/2026-10-12?tolerance_m=0", "tolerance_m"),
        ("/api/v1/days/2026-10-12?tolerance_m=ten", "tolerance_m"),
        ("/api/v1/days/yesterday", "date"),
    ] {
        let (status, body) = get(&router, &me, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"]["field"], field, "{path}");
    }
}

#[tokio::test]
async fn points_page_completely_and_stably_across_two_devices() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let second = another_device(&router, &me).await;
    let start = at("2026-10-12T06:00:00Z");
    let first_batch: Vec<Value> = (0..6_000)
        .map(|i| point(start + Duration::seconds(i), i % 50, 8, false))
        .collect();
    // The second phone shares the first hundred seconds with the first one.
    let second_batch: Vec<Value> = (0..6_000)
        .map(|i| {
            let offset = if i < 100 { i } else { 10_000 + i };
            point(start + Duration::seconds(offset), i % 50, 8, false)
        })
        .collect();
    upload(&router, &me, first_batch).await;
    upload(&router, &second, second_batch).await;

    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            None => "/api/v1/points?from=2026-10-12T00:00:00Z&limit=5000".to_string(),
            Some(c) => format!("/api/v1/points?from=2026-10-12T00:00:00Z&limit=5000&cursor={c}"),
        };
        let (status, body) = get(&router, &me, &path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        pages += 1;
        for p in body["points"].as_array().unwrap() {
            let key = (
                p["device_id"].as_str().unwrap().to_string(),
                p["recorded_at"].as_str().unwrap().to_string(),
            );
            assert!(seen.insert(key), "a point came twice");
            assert!(p.get("user_id").is_none());
            assert!(p.get("import_batch_id").is_some());
        }
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen.len(), 12_000);

    for (path, field) in [
        (
            "/api/v1/points?from=2026-10-12T00:00:00Z&limit=20001",
            "limit",
        ),
        (
            "/api/v1/points?from=2026-10-12T00:00:00Z&cursor=xx",
            "cursor",
        ),
        ("/api/v1/points?to=2026-10-13T00:00:00Z", "from"),
        ("/api/v1/points?from=yesterday", "from"),
    ] {
        let (status, body) = get(&router, &me, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"]["field"], field, "{path}");
    }
}

#[tokio::test]
async fn changes_stream_the_four_tables_by_the_servers_clock() {
    let (router, pool) = test_app().await;
    let me = signed_in(&router, &pool).await;
    let other = signed_in(&router, &pool).await;
    let before = rfc(Utc::now() - Duration::minutes(1));
    let place = json!({
        "id": Uuid::now_v7(), "name": "Home", "lat": 522297700, "lon": 210122200,
        // A row that arrives late: its phone clock is years old, its synced_at is now.
        "updated_at": "2020-01-01T00:00:00Z"
    });
    let marker = json!({
        "id": Uuid::now_v7(), "recorded_at": "2026-10-12T10:05:00Z", "tz": "Europe/Warsaw",
        "lat": 522297700, "lon": 210122200, "note": "Keys", "updated_at": "2026-10-12T10:05:00Z"
    });
    let correction = json!({
        "id": Uuid::now_v7(), "kind": "retype", "start_at": "2026-10-12T10:00:00Z",
        "end_at": "2026-10-12T10:20:00Z", "payload": {"mode_type": "bike"}, "updated_at": "2026-10-12T10:30:00Z"
    });
    let segment = stop("2026-10-12T10:00:00Z", "2026-10-12T10:20:00Z");
    put(
        &router,
        &me,
        "/api/v1/places",
        "places",
        vec![place.clone()],
    )
    .await;
    put(
        &router,
        &me,
        "/api/v1/markers",
        "markers",
        vec![marker.clone()],
    )
    .await;
    put(
        &router,
        &me,
        "/api/v1/corrections",
        "corrections",
        vec![correction.clone()],
    )
    .await;
    put(
        &router,
        &me,
        "/api/v1/segments",
        "segments",
        vec![segment.clone()],
    )
    .await;
    // A server segment, which the stream never carries.
    sqlx::query(
        "insert into segments (user_id, kind, start_at, end_at, tz, mode_detected, mode_conf, mode_type,
            center_lat, center_lon, source, computed_by, algorithm_version)
         values ($1, 'stop', now(), now(), 'Europe/Warsaw', 'stationary', 'high', 'other', 1, 1, 'phone', 'server', 1)",
    )
    .bind(me.user_id)
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = get(
        &router,
        &me,
        &format!("/api/v1/sync/changes?since={before}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["segments"].as_array().unwrap().len(), 1);
    assert_eq!(body["segments"][0]["id"], segment["id"]);
    assert_eq!(body["corrections"][0]["id"], correction["id"]);
    assert_eq!(
        body["corrections"][0]["payload"],
        json!({"mode_type": "bike"})
    );
    assert_eq!(body["places"][0]["id"], place["id"]);
    assert_eq!(body["places"][0]["updated_at"], "2020-01-01T00:00:00Z");
    assert_eq!(body["markers"][0]["id"], marker["id"]);
    assert_eq!(body["next_cursor"], Value::Null);
    let server_time = body["server_time"].as_str().unwrap().to_string();
    assert!(body["places"][0].get("user_id").is_none());

    // Two rows a page: every row once over the pages, in the server's order.
    let mut ids: Vec<String> = vec![];
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let path = match &cursor {
            None => format!("/api/v1/sync/changes?since={before}&limit=2"),
            Some(c) => format!("/api/v1/sync/changes?since={before}&limit=2&cursor={c}"),
        };
        let (status, body) = get(&router, &me, &path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut on_page = 0;
        for table in ["segments", "corrections", "places", "markers"] {
            for row in body[table].as_array().unwrap() {
                ids.push(row["id"].as_str().unwrap().to_string());
                on_page += 1;
            }
        }
        assert!(on_page <= 2, "{body}");
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);

    // From the first page's server_time on, nothing is new until the next upload.
    let (_, body) = get(
        &router,
        &me,
        &format!("/api/v1/sync/changes?since={server_time}"),
    )
    .await;
    assert_eq!(body["segments"], json!([]));
    assert_eq!(body["places"], json!([]));
    assert_eq!(body["next_cursor"], Value::Null);
    let mut renamed = place.clone();
    renamed["name"] = json!("Dom");
    renamed["updated_at"] = json!("2020-01-02T00:00:00Z");
    put(&router, &me, "/api/v1/places", "places", vec![renamed]).await;
    let (_, body) = get(
        &router,
        &me,
        &format!("/api/v1/sync/changes?since={server_time}"),
    )
    .await;
    assert_eq!(body["places"][0]["name"], "Dom");

    let (_, body) = get(
        &router,
        &other,
        &format!("/api/v1/sync/changes?since={before}"),
    )
    .await;
    assert_eq!(body["places"], json!([]));
    for (path, field) in [
        ("/api/v1/sync/changes", "since"),
        ("/api/v1/sync/changes?since=now", "since"),
        (
            &*format!("/api/v1/sync/changes?since={before}&limit=5001"),
            "limit",
        ),
        (
            &*format!("/api/v1/sync/changes?since={before}&cursor=xx"),
            "cursor",
        ),
    ] {
        let (status, body) = get(&router, &me, path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"]["field"], field, "{path}");
    }
}
