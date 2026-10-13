//! The ingest API: the phone's deferred uploads. Every write here is safe to repeat, as the
//! phone retries a batch whose answer it lost: a replayed batch id stores nothing and
//! answers as the first time did, a point already there is a duplicate and is never
//! changed, and an upsert applies a row only when the phone's `updated_at` is newer.

use std::collections::HashSet;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::AuthUser;
use crate::errors::ApiError;

/// A body over this is refused before it is read to the end.
pub const BODY_LIMIT_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_POINTS: usize = 100_000;
pub const MAX_ROWS: usize = 10_000;
/// The five routes share this limit per account, a sliding minute.
pub const REQUESTS_PER_MINUTE: usize = 120;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/points/batches", post(points_batch))
        .route("/api/v1/segments", put(put_segments))
        .route("/api/v1/corrections", put(put_corrections))
        .route("/api/v1/places", put(put_places))
        .route("/api/v1/markers", put(put_markers))
        .layer(DefaultBodyLimit::max(BODY_LIMIT_BYTES))
}

// Shared

fn json_error(rejection: JsonRejection) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "the body is over 5 MB",
        )
    } else {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            rejection.body_text(),
        )
    }
}

fn check_limit(state: &AppState, user: &AuthUser) -> Result<(), ApiError> {
    state
        .ingest_limiter
        .hit(
            &user.user_id.to_string(),
            REQUESTS_PER_MINUTE,
            Duration::from_secs(60),
        )
        .map_err(ApiError::rate_limited)
}

fn forbidden(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden", message)
}

/// Whether the device is the user's.
async fn device_belongs(
    conn: &mut PgConnection,
    device_id: Uuid,
    user_id: Uuid,
) -> Result<bool, ApiError> {
    let owner: Option<Uuid> = sqlx::query_scalar("select user_id from devices where id = $1")
        .bind(device_id)
        .fetch_optional(conn)
        .await?;
    Ok(owner == Some(user_id))
}

/// Whether the place is the user's, for a segment's `place_id`.
async fn place_belongs(
    conn: &mut PgConnection,
    place_id: Uuid,
    user_id: Uuid,
) -> Result<bool, ApiError> {
    let owner: Option<Uuid> = sqlx::query_scalar("select user_id from places where id = $1")
        .bind(place_id)
        .fetch_optional(conn)
        .await?;
    Ok(owner == Some(user_id))
}

/// 403 when any of the ids is a row of another user; the whole request is then refused.
async fn refuse_foreign_rows(
    conn: &mut PgConnection,
    table: &'static str,
    ids: &[Uuid],
    user_id: Uuid,
) -> Result<(), ApiError> {
    let sql = format!("select count(*) from {table} where id = any($1) and user_id <> $2");
    let foreign: i64 = sqlx::query_scalar(&sql)
        .bind(ids)
        .bind(user_id)
        .fetch_one(conn)
        .await?;
    if foreign > 0 {
        return Err(forbidden("a row id belongs to another account"));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct UpsertCounts {
    pub applied: usize,
    pub ignored: usize,
}

fn count(applied: usize, total: usize) -> Json<UpsertCounts> {
    Json(UpsertCounts {
        applied,
        ignored: total - applied,
    })
}

fn parse_time(
    index: usize,
    field: &str,
    value: Option<&str>,
    code: &'static str,
) -> Result<DateTime<Utc>, ApiError> {
    let Some(text) = value else {
        return Err(ApiError::invalid_row(
            code,
            index,
            field,
            format!("{field} is required"),
        ));
    };
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| {
            ApiError::invalid_row(
                code,
                index,
                field,
                format!("{field} is not an RFC 3339 time"),
            )
        })
}

fn parse_tz(index: usize, value: Option<&str>, code: &'static str) -> Result<String, ApiError> {
    let Some(tz) = value else {
        return Err(ApiError::invalid_row(code, index, "tz", "tz is required"));
    };
    if tz.is_empty() || tz.len() > 64 || tz.parse::<Tz>().is_err() {
        return Err(ApiError::invalid_row(
            code,
            index,
            "tz",
            "tz is not a known IANA zone name",
        ));
    }
    Ok(tz.to_string())
}

fn in_range(
    index: usize,
    field: &str,
    value: i64,
    low: i64,
    high: i64,
    code: &'static str,
) -> Result<i64, ApiError> {
    if value < low || value > high {
        return Err(ApiError::invalid_row(
            code,
            index,
            field,
            format!("{field} must be between {low} and {high}"),
        ));
    }
    Ok(value)
}

fn required<T>(
    index: usize,
    field: &str,
    value: Option<T>,
    code: &'static str,
) -> Result<T, ApiError> {
    value.ok_or_else(|| ApiError::invalid_row(code, index, field, format!("{field} is required")))
}

fn one_of(
    index: usize,
    field: &str,
    value: &str,
    allowed: &[&str],
    code: &'static str,
) -> Result<(), ApiError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(ApiError::invalid_row(
            code,
            index,
            field,
            format!("{field} must be one of {}", allowed.join(", ")),
        ))
    }
}

fn lat(index: usize, field: &str, value: i64, code: &'static str) -> Result<i32, ApiError> {
    Ok(in_range(index, field, value, -900_000_000, 900_000_000, code)? as i32)
}

fn lon(index: usize, field: &str, value: i64, code: &'static str) -> Result<i32, ApiError> {
    Ok(in_range(index, field, value, -1_800_000_000, 1_800_000_000, code)? as i32)
}

fn optional_time(
    index: usize,
    field: &str,
    value: Option<&str>,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    value
        .map(|v| parse_time(index, field, Some(v), ROW))
        .transpose()
}

// Points

#[derive(Deserialize)]
pub struct BatchRequest {
    pub batch_id: Uuid,
    pub device_id: Uuid,
    pub points: Vec<PointIn>,
}

/// A point as sent, every number wide and every field optional, so the ranges are checked
/// here and named by index and field rather than refused by serde.
#[derive(Deserialize)]
pub struct PointIn {
    pub recorded_at: Option<String>,
    pub tz: Option<String>,
    pub lat: Option<i64>,
    pub lon: Option<i64>,
    pub h_acc_m: Option<i64>,
    pub alt_m: Option<i64>,
    pub v_acc_m: Option<i64>,
    pub speed_cms: Option<i64>,
    pub course_deg: Option<i64>,
    pub activity: Option<i64>,
    pub activity_conf: Option<i64>,
    pub source: Option<String>,
    pub battery_pct: Option<i64>,
    pub battery_state: Option<i64>,
    pub coarse: Option<bool>,
    pub import_batch_id: Option<Uuid>,
}

#[derive(Debug)]
struct PointRow {
    recorded_at: DateTime<Utc>,
    tz: String,
    lat: i32,
    lon: i32,
    h_acc_m: i16,
    alt_m: Option<i16>,
    v_acc_m: Option<i16>,
    speed_cms: i16,
    course_deg: i16,
    activity: i16,
    activity_conf: i16,
    source: String,
    battery_pct: Option<i16>,
    battery_state: Option<i16>,
    coarse: bool,
}

const POINT: &str = "invalid_point";
const ROW: &str = "invalid_row";

fn validate_point(index: usize, p: &PointIn, now: DateTime<Utc>) -> Result<PointRow, ApiError> {
    let recorded_at = parse_time(index, "recorded_at", p.recorded_at.as_deref(), POINT)?;
    let earliest = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
    if recorded_at < earliest || recorded_at > now + chrono::Duration::minutes(5) {
        return Err(ApiError::invalid_row(
            POINT,
            index,
            "recorded_at",
            "recorded_at must be after 2000-01-01 and not more than 5 minutes ahead",
        ));
    }
    let tz = parse_tz(index, p.tz.as_deref(), POINT)?;
    let lat = lat(index, "lat", required(index, "lat", p.lat, POINT)?, POINT)?;
    let lon = lon(index, "lon", required(index, "lon", p.lon, POINT)?, POINT)?;
    let h_acc_m = in_range(
        index,
        "h_acc_m",
        required(index, "h_acc_m", p.h_acc_m, POINT)?,
        0,
        32_767,
        POINT,
    )? as i16;
    let v_acc_m = p
        .v_acc_m
        .map(|v| in_range(index, "v_acc_m", v, 0, 32_767, POINT))
        .transpose()?
        .map(|v| v as i16);
    let alt_m = p
        .alt_m
        .map(|v| in_range(index, "alt_m", v, -1_000, 32_767, POINT))
        .transpose()?
        .map(|v| v as i16);
    let speed_cms = in_range(
        index,
        "speed_cms",
        p.speed_cms.unwrap_or(-1),
        -1,
        32_767,
        POINT,
    )? as i16;
    let course_deg = in_range(
        index,
        "course_deg",
        p.course_deg.unwrap_or(-1),
        -1,
        359,
        POINT,
    )? as i16;
    let activity = in_range(index, "activity", p.activity.unwrap_or(0), 0, 5, POINT)? as i16;
    let activity_conf = in_range(
        index,
        "activity_conf",
        p.activity_conf.unwrap_or(0),
        0,
        2,
        POINT,
    )? as i16;
    let source = p.source.clone().unwrap_or_else(|| "phone".to_string());
    one_of(
        index,
        "source",
        &source,
        &["phone", "visit", "significant_change"],
        POINT,
    )?;
    let battery_pct = p
        .battery_pct
        .map(|v| in_range(index, "battery_pct", v, 0, 100, POINT))
        .transpose()?
        .map(|v| v as i16);
    let battery_state = p
        .battery_state
        .map(|v| in_range(index, "battery_state", v, 0, 3, POINT))
        .transpose()?
        .map(|v| v as i16);
    let coarse = required(index, "coarse", p.coarse, POINT)?;
    if p.import_batch_id.is_some() {
        return Err(ApiError::invalid_row(
            POINT,
            index,
            "import_batch_id",
            "import batches go through the import API",
        ));
    }
    Ok(PointRow {
        recorded_at,
        tz,
        lat,
        lon,
        h_acc_m,
        alt_m,
        v_acc_m,
        speed_cms,
        course_deg,
        activity,
        activity_conf,
        source,
        battery_pct,
        battery_state,
        coarse,
    })
}

#[derive(Serialize)]
pub struct BatchCounts {
    pub accepted: i32,
    pub duplicates: i32,
}

async fn points_batch(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Json<BatchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<BatchCounts>), ApiError> {
    check_limit(&state, &user)?;
    let Json(body) = body.map_err(json_error)?;
    if body.points.is_empty() || body.points.len() > MAX_POINTS {
        return Err(ApiError::validation(format!(
            "points must hold 1 to {MAX_POINTS} entries"
        )));
    }
    let now = Utc::now();
    let mut rows = Vec::with_capacity(body.points.len());
    for (index, point) in body.points.iter().enumerate() {
        rows.push(validate_point(index, point, now)?);
    }
    // The first of two points with the same time is the one that counts, as the database
    // would keep whichever came first anyway.
    let mut seen = HashSet::with_capacity(rows.len());
    rows.retain(|row| seen.insert(row.recorded_at));
    let sent = body.points.len() as i32;

    let mut tx = state.pool.begin().await?;
    if !device_belongs(&mut tx, body.device_id, user.user_id).await? {
        return Err(forbidden("the device is not this account's"));
    }
    let earlier: Option<(Uuid, Uuid, i32, i32)> = sqlx::query_as(
        "select b.device_id, d.user_id, b.accepted, b.duplicates
         from upload_batches b join devices d on d.id = b.device_id where b.id = $1",
    )
    .bind(body.batch_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((device_id, user_id, accepted, duplicates)) = earlier {
        if device_id != body.device_id || user_id != user.user_id {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "batch_id_taken",
                "the batch id was used by another device",
            ));
        }
        return Ok((
            StatusCode::OK,
            Json(BatchCounts {
                accepted,
                duplicates,
            }),
        ));
    }

    let inserted = sqlx::query(
        "insert into points (user_id, device_id, recorded_at, tz, lat, lon, h_acc_m, alt_m, v_acc_m,
            speed_cms, course_deg, activity, activity_conf, source, battery_pct, battery_state, coarse)
         select $1, $2, * from unnest(
            $3::timestamptz[], $4::text[], $5::int4[], $6::int4[], $7::int2[], $8::int2[], $9::int2[],
            $10::int2[], $11::int2[], $12::int2[], $13::int2[], $14::text[], $15::int2[], $16::int2[], $17::bool[])
         on conflict (device_id, recorded_at) do nothing",
    )
    .bind(user.user_id)
    .bind(body.device_id)
    .bind(rows.iter().map(|r| r.recorded_at).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.tz.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.lat).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.lon).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.h_acc_m).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.alt_m).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.v_acc_m).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.speed_cms).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.course_deg).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.activity).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.activity_conf).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.source.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.battery_pct).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.battery_state).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.coarse).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
    let accepted = inserted.rows_affected() as i32;
    let duplicates = sent - accepted;
    sqlx::query(
        "insert into upload_batches (id, device_id, point_count, accepted, duplicates)
         values ($1, $2, $3, $4, $5)",
    )
    .bind(body.batch_id)
    .bind(body.device_id)
    .bind(sent)
    .bind(accepted)
    .bind(duplicates)
    .execute(&mut *tx)
    .await?;
    sqlx::query("update devices set last_seen_at = now() where id = $1")
        .bind(body.device_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(
        user_id = %user.user_id, device_id = %body.device_id, batch_id = %body.batch_id,
        accepted, duplicates, "batch stored"
    );
    Ok((
        StatusCode::CREATED,
        Json(BatchCounts {
            accepted,
            duplicates,
        }),
    ))
}

// Segments

#[derive(Deserialize)]
pub struct SegmentsRequest {
    pub segments: Vec<SegmentIn>,
}

#[derive(Deserialize)]
pub struct SegmentIn {
    pub id: Uuid,
    pub device_id: Option<Uuid>,
    pub kind: Option<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub tz: Option<String>,
    pub mode_detected: Option<String>,
    pub mode_conf: Option<String>,
    pub mode_type: Option<String>,
    pub distance_m: Option<i64>,
    pub point_count: Option<i64>,
    pub center_lat: Option<i64>,
    pub center_lon: Option<i64>,
    pub place_id: Option<Uuid>,
    pub source: Option<String>,
    pub computed_by: Option<String>,
    pub algorithm_version: Option<i64>,
    pub steps: Option<i64>,
    pub coarse: Option<bool>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

fn count_rows<T>(rows: &[T]) -> Result<(), ApiError> {
    if rows.is_empty() || rows.len() > MAX_ROWS {
        return Err(ApiError::validation(format!(
            "the request must hold 1 to {MAX_ROWS} rows"
        )));
    }
    Ok(())
}

fn segment_center(
    index: usize,
    kind: &str,
    center_lat: Option<i64>,
    center_lon: Option<i64>,
) -> Result<(Option<i32>, Option<i32>), ApiError> {
    match (kind, center_lat, center_lon) {
        ("stop", Some(la), Some(lo)) => Ok((
            Some(lat(index, "center_lat", la, ROW)?),
            Some(lon(index, "center_lon", lo, ROW)?),
        )),
        ("stop", None, _) => Err(ApiError::invalid_row(
            ROW,
            index,
            "center_lat",
            "a stop needs its center",
        )),
        ("stop", _, None) => Err(ApiError::invalid_row(
            ROW,
            index,
            "center_lon",
            "a stop needs its center",
        )),
        (_, None, None) => Ok((None, None)),
        (_, Some(_), _) => Err(ApiError::invalid_row(
            ROW,
            index,
            "center_lat",
            "a trip has no center",
        )),
        (_, _, Some(_)) => Err(ApiError::invalid_row(
            ROW,
            index,
            "center_lon",
            "a trip has no center",
        )),
    }
}

async fn put_segments(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Json<SegmentsRequest>, JsonRejection>,
) -> Result<Json<UpsertCounts>, ApiError> {
    check_limit(&state, &user)?;
    let Json(body) = body.map_err(json_error)?;
    count_rows(&body.segments)?;
    let mut tx = state.pool.begin().await?;
    let ids: Vec<Uuid> = body.segments.iter().map(|s| s.id).collect();
    refuse_foreign_rows(&mut tx, "segments", &ids, user.user_id).await?;
    let mut applied = 0;
    for (index, s) in body.segments.iter().enumerate() {
        let kind = required(index, "kind", s.kind.as_deref(), ROW)?;
        one_of(index, "kind", kind, &["stop", "trip"], ROW)?;
        let start_at = parse_time(index, "start_at", s.start_at.as_deref(), ROW)?;
        let end_at = parse_time(index, "end_at", s.end_at.as_deref(), ROW)?;
        if start_at > end_at {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "end_at",
                "end_at must not be before start_at",
            ));
        }
        let tz = parse_tz(index, s.tz.as_deref(), ROW)?;
        let mode_detected = required(index, "mode_detected", s.mode_detected.as_deref(), ROW)?;
        one_of(
            index,
            "mode_detected",
            mode_detected,
            &[
                "stationary",
                "walking",
                "running",
                "cycling",
                "automotive",
                "unknown",
            ],
            ROW,
        )?;
        let mode_conf = required(index, "mode_conf", s.mode_conf.as_deref(), ROW)?;
        one_of(
            index,
            "mode_conf",
            mode_conf,
            &["low", "medium", "high"],
            ROW,
        )?;
        let mode_type = required(index, "mode_type", s.mode_type.as_deref(), ROW)?;
        one_of(
            index,
            "mode_type",
            mode_type,
            &[
                "walk",
                "run",
                "scooter",
                "bike",
                "motorcycle",
                "car",
                "train",
                "plane",
                "other",
            ],
            ROW,
        )?;
        let distance_m = in_range(
            index,
            "distance_m",
            s.distance_m.unwrap_or(0),
            0,
            i32::MAX as i64,
            ROW,
        )? as i32;
        let point_count = in_range(
            index,
            "point_count",
            s.point_count.unwrap_or(0),
            0,
            i32::MAX as i64,
            ROW,
        )? as i32;
        let (center_lat, center_lon) = segment_center(index, kind, s.center_lat, s.center_lon)?;
        let source = s.source.clone().unwrap_or_else(|| "phone".to_string());
        if source.is_empty() || source.len() > 64 {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "source",
                "source is 1 to 64 characters",
            ));
        }
        let computed_by = s.computed_by.as_deref().unwrap_or("phone");
        if computed_by != "phone" {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "computed_by",
                "only phone segments are uploaded",
            ));
        }
        let algorithm_version = in_range(
            index,
            "algorithm_version",
            s.algorithm_version.unwrap_or(1),
            1,
            32_767,
            ROW,
        )? as i16;
        let steps = s
            .steps
            .map(|v| in_range(index, "steps", v, 0, 10_000_000, ROW))
            .transpose()?
            .map(|v| v as i32);
        let coarse = s.coarse.unwrap_or(false);
        let updated_at = parse_time(index, "updated_at", s.updated_at.as_deref(), ROW)?;
        let deleted_at = optional_time(index, "deleted_at", s.deleted_at.as_deref())?;
        if let Some(device_id) = s.device_id
            && !device_belongs(&mut tx, device_id, user.user_id).await?
        {
            return Err(forbidden("the device is not this account's"));
        }
        if let Some(place_id) = s.place_id
            && !place_belongs(&mut tx, place_id, user.user_id).await?
        {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "place_id",
                "the place is not this account's",
            ));
        }
        let row: Option<Uuid> = sqlx::query_scalar(
            "insert into segments (id, user_id, device_id, kind, start_at, end_at, tz, mode_detected, mode_conf,
                mode_type, distance_m, point_count, center_lat, center_lon, place_id, source, computed_by,
                algorithm_version, steps, coarse, updated_at, synced_at, deleted_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                $21, now(), $22)
             on conflict (id) do update set device_id = excluded.device_id, kind = excluded.kind,
                start_at = excluded.start_at, end_at = excluded.end_at, tz = excluded.tz,
                mode_detected = excluded.mode_detected, mode_conf = excluded.mode_conf,
                mode_type = excluded.mode_type, distance_m = excluded.distance_m,
                point_count = excluded.point_count, center_lat = excluded.center_lat,
                center_lon = excluded.center_lon, place_id = excluded.place_id, source = excluded.source,
                computed_by = excluded.computed_by, algorithm_version = excluded.algorithm_version,
                steps = excluded.steps, coarse = excluded.coarse, updated_at = excluded.updated_at,
                synced_at = now(), deleted_at = excluded.deleted_at
             where segments.updated_at < excluded.updated_at
             returning id",
        )
        .bind(s.id)
        .bind(user.user_id)
        .bind(s.device_id)
        .bind(kind)
        .bind(start_at)
        .bind(end_at)
        .bind(&tz)
        .bind(mode_detected)
        .bind(mode_conf)
        .bind(mode_type)
        .bind(distance_m)
        .bind(point_count)
        .bind(center_lat)
        .bind(center_lon)
        .bind(s.place_id)
        .bind(&source)
        .bind(computed_by)
        .bind(algorithm_version)
        .bind(steps)
        .bind(coarse)
        .bind(updated_at)
        .bind(deleted_at)
        .fetch_optional(&mut *tx)
        .await?;
        applied += usize::from(row.is_some());
    }
    tx.commit().await?;
    Ok(count(applied, body.segments.len()))
}

// Corrections

#[derive(Deserialize)]
pub struct CorrectionsRequest {
    pub corrections: Vec<CorrectionIn>,
}

#[derive(Deserialize)]
pub struct CorrectionIn {
    pub id: Uuid,
    pub kind: Option<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub payload: Option<Value>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

async fn put_corrections(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Json<CorrectionsRequest>, JsonRejection>,
) -> Result<Json<UpsertCounts>, ApiError> {
    check_limit(&state, &user)?;
    let Json(body) = body.map_err(json_error)?;
    count_rows(&body.corrections)?;
    let mut tx = state.pool.begin().await?;
    let ids: Vec<Uuid> = body.corrections.iter().map(|c| c.id).collect();
    refuse_foreign_rows(&mut tx, "corrections", &ids, user.user_id).await?;
    let mut applied = 0;
    for (index, c) in body.corrections.iter().enumerate() {
        let kind = required(index, "kind", c.kind.as_deref(), ROW)?;
        one_of(
            index,
            "kind",
            kind,
            &["retype", "split", "merge", "promote_stop", "delete"],
            ROW,
        )?;
        let start_at = parse_time(index, "start_at", c.start_at.as_deref(), ROW)?;
        let end_at = parse_time(index, "end_at", c.end_at.as_deref(), ROW)?;
        if start_at > end_at {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "end_at",
                "end_at must not be before start_at",
            ));
        }
        let payload = c
            .payload
            .clone()
            .unwrap_or_else(|| Value::Object(Default::default()));
        if !payload.is_object() {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "payload",
                "payload must be a JSON object",
            ));
        }
        let created_at = optional_time(index, "created_at", c.created_at.as_deref())?;
        let updated_at = parse_time(index, "updated_at", c.updated_at.as_deref(), ROW)?;
        let deleted_at = optional_time(index, "deleted_at", c.deleted_at.as_deref())?;
        let row: Option<Uuid> = sqlx::query_scalar(
            "insert into corrections (id, user_id, kind, start_at, end_at, payload, created_at, updated_at,
                synced_at, deleted_at)
             values ($1, $2, $3, $4, $5, $6, coalesce($7, now()), $8, now(), $9)
             on conflict (id) do update set kind = excluded.kind, start_at = excluded.start_at,
                end_at = excluded.end_at, payload = excluded.payload, updated_at = excluded.updated_at,
                synced_at = now(), deleted_at = excluded.deleted_at
             where corrections.updated_at < excluded.updated_at
             returning id",
        )
        .bind(c.id)
        .bind(user.user_id)
        .bind(kind)
        .bind(start_at)
        .bind(end_at)
        .bind(&payload)
        .bind(created_at)
        .bind(updated_at)
        .bind(deleted_at)
        .fetch_optional(&mut *tx)
        .await?;
        applied += usize::from(row.is_some());
    }
    tx.commit().await?;
    Ok(count(applied, body.corrections.len()))
}

// Places

#[derive(Deserialize)]
pub struct PlacesRequest {
    pub places: Vec<PlaceIn>,
}

#[derive(Deserialize)]
pub struct PlaceIn {
    pub id: Uuid,
    pub name: Option<String>,
    pub lat: Option<i64>,
    pub lon: Option<i64>,
    pub radius_m: Option<i64>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

async fn put_places(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Json<PlacesRequest>, JsonRejection>,
) -> Result<Json<UpsertCounts>, ApiError> {
    check_limit(&state, &user)?;
    let Json(body) = body.map_err(json_error)?;
    count_rows(&body.places)?;
    let mut tx = state.pool.begin().await?;
    let ids: Vec<Uuid> = body.places.iter().map(|p| p.id).collect();
    refuse_foreign_rows(&mut tx, "places", &ids, user.user_id).await?;
    let mut applied = 0;
    for (index, p) in body.places.iter().enumerate() {
        let name = required(index, "name", p.name.as_deref(), ROW)?;
        let length = name.chars().count();
        if length == 0 || length > 200 {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "name",
                "name is 1 to 200 characters",
            ));
        }
        let lat = lat(index, "lat", required(index, "lat", p.lat, ROW)?, ROW)?;
        let lon = lon(index, "lon", required(index, "lon", p.lon, ROW)?, ROW)?;
        let radius_m =
            in_range(index, "radius_m", p.radius_m.unwrap_or(100), 10, 5_000, ROW)? as i16;
        let created_at = optional_time(index, "created_at", p.created_at.as_deref())?;
        let updated_at = parse_time(index, "updated_at", p.updated_at.as_deref(), ROW)?;
        let deleted_at = optional_time(index, "deleted_at", p.deleted_at.as_deref())?;
        let row: Option<Uuid> = sqlx::query_scalar(
            "insert into places (id, user_id, name, lat, lon, radius_m, created_at, updated_at, synced_at,
                deleted_at)
             values ($1, $2, $3, $4, $5, $6, coalesce($7, now()), $8, now(), $9)
             on conflict (id) do update set name = excluded.name, lat = excluded.lat, lon = excluded.lon,
                radius_m = excluded.radius_m, updated_at = excluded.updated_at, synced_at = now(),
                deleted_at = excluded.deleted_at
             where places.updated_at < excluded.updated_at
             returning id",
        )
        .bind(p.id)
        .bind(user.user_id)
        .bind(name)
        .bind(lat)
        .bind(lon)
        .bind(radius_m)
        .bind(created_at)
        .bind(updated_at)
        .bind(deleted_at)
        .fetch_optional(&mut *tx)
        .await?;
        applied += usize::from(row.is_some());
    }
    tx.commit().await?;
    Ok(count(applied, body.places.len()))
}

// Markers

#[derive(Deserialize)]
pub struct MarkersRequest {
    pub markers: Vec<MarkerIn>,
}

#[derive(Deserialize)]
pub struct MarkerIn {
    pub id: Uuid,
    pub recorded_at: Option<String>,
    pub tz: Option<String>,
    pub lat: Option<i64>,
    pub lon: Option<i64>,
    pub note: Option<String>,
    pub photo_ref: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
}

async fn put_markers(
    State(state): State<AppState>,
    user: AuthUser,
    body: Result<Json<MarkersRequest>, JsonRejection>,
) -> Result<Json<UpsertCounts>, ApiError> {
    check_limit(&state, &user)?;
    let Json(body) = body.map_err(json_error)?;
    count_rows(&body.markers)?;
    let mut tx = state.pool.begin().await?;
    let ids: Vec<Uuid> = body.markers.iter().map(|m| m.id).collect();
    refuse_foreign_rows(&mut tx, "markers", &ids, user.user_id).await?;
    let mut applied = 0;
    for (index, m) in body.markers.iter().enumerate() {
        let recorded_at = parse_time(index, "recorded_at", m.recorded_at.as_deref(), ROW)?;
        let tz = parse_tz(index, m.tz.as_deref(), ROW)?;
        let lat = lat(index, "lat", required(index, "lat", m.lat, ROW)?, ROW)?;
        let lon = lon(index, "lon", required(index, "lon", m.lon, ROW)?, ROW)?;
        let note = m.note.clone().unwrap_or_default();
        if note.chars().count() > 2_000 {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "note",
                "note is at most 2000 characters",
            ));
        }
        if let Some(photo_ref) = &m.photo_ref
            && photo_ref.chars().count() > 512
        {
            return Err(ApiError::invalid_row(
                ROW,
                index,
                "photo_ref",
                "photo_ref is at most 512 characters",
            ));
        }
        let created_at = optional_time(index, "created_at", m.created_at.as_deref())?;
        let updated_at = parse_time(index, "updated_at", m.updated_at.as_deref(), ROW)?;
        let deleted_at = optional_time(index, "deleted_at", m.deleted_at.as_deref())?;
        let row: Option<Uuid> = sqlx::query_scalar(
            "insert into markers (id, user_id, recorded_at, tz, lat, lon, note, photo_ref, created_at,
                updated_at, synced_at, deleted_at)
             values ($1, $2, $3, $4, $5, $6, $7, $8, coalesce($9, now()), $10, now(), $11)
             on conflict (id) do update set recorded_at = excluded.recorded_at, tz = excluded.tz,
                lat = excluded.lat, lon = excluded.lon, note = excluded.note, photo_ref = excluded.photo_ref,
                updated_at = excluded.updated_at, synced_at = now(), deleted_at = excluded.deleted_at
             where markers.updated_at < excluded.updated_at
             returning id",
        )
        .bind(m.id)
        .bind(user.user_id)
        .bind(recorded_at)
        .bind(&tz)
        .bind(lat)
        .bind(lon)
        .bind(&note)
        .bind(&m.photo_ref)
        .bind(created_at)
        .bind(updated_at)
        .bind(deleted_at)
        .fetch_optional(&mut *tx)
        .await?;
        applied += usize::from(row.is_some());
    }
    tx.commit().await?;
    Ok(count(applied, body.markers.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point() -> PointIn {
        PointIn {
            recorded_at: Some("2026-10-12T12:00:00Z".into()),
            tz: Some("Europe/Warsaw".into()),
            lat: Some(522_297_700),
            lon: Some(210_122_200),
            h_acc_m: Some(8),
            alt_m: None,
            v_acc_m: None,
            speed_cms: None,
            course_deg: None,
            activity: Some(2),
            activity_conf: Some(2),
            source: None,
            battery_pct: Some(80),
            battery_state: Some(1),
            coarse: Some(false),
            import_batch_id: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap()
    }

    #[test]
    fn a_point_inside_every_range_passes_with_the_defaults_filled() {
        let row = validate_point(0, &point(), now()).unwrap();
        assert_eq!(row.speed_cms, -1);
        assert_eq!(row.course_deg, -1);
        assert_eq!(row.source, "phone");
        assert_eq!(row.lat, 522_297_700);
    }

    #[test]
    fn the_first_failing_field_is_named_with_its_index() {
        let mut p = point();
        p.lon = Some(1_800_000_001);
        let error = validate_point(3, &p, now()).unwrap_err();
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.code, "invalid_point");
        assert_eq!(error.index, Some(3));
        assert_eq!(error.field.as_deref(), Some("lon"));
    }

    #[test]
    fn a_time_too_far_ahead_or_before_2000_is_refused() {
        let mut p = point();
        p.recorded_at = Some("2026-10-12T12:06:00Z".into());
        assert_eq!(
            validate_point(0, &p, now()).unwrap_err().field.as_deref(),
            Some("recorded_at")
        );
        p.recorded_at = Some("1999-12-31T23:59:59Z".into());
        assert_eq!(
            validate_point(0, &p, now()).unwrap_err().field.as_deref(),
            Some("recorded_at")
        );
        p.recorded_at = Some("2026-10-12T14:04:00+02:00".into());
        assert!(validate_point(0, &p, now()).is_ok());
    }

    #[test]
    fn an_import_source_or_batch_is_sent_elsewhere() {
        let mut p = point();
        p.source = Some("import:gpx".into());
        assert_eq!(
            validate_point(0, &p, now()).unwrap_err().field.as_deref(),
            Some("source")
        );
        let mut p = point();
        p.import_batch_id = Some(Uuid::now_v7());
        assert_eq!(
            validate_point(0, &p, now()).unwrap_err().field.as_deref(),
            Some("import_batch_id")
        );
    }
}
