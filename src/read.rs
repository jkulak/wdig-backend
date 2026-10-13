//! The read API: what a new phone restores from, what the comparison view draws, and what a
//! second device catches up with. Days are each row's own local day; cursors are the last
//! row's sort key, so a page never skips or repeats a row while new ones arrive.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::app::AppState;
use crate::auth::AuthUser;
use crate::errors::ApiError;
use crate::geo::{Geo, simplify};

pub const MAX_RANGE_DAYS: i64 = 366;
pub const DEFAULT_TOLERANCE_M: f64 = 10.0;
pub const POINTS_PAGE_DEFAULT: i64 = 5_000;
pub const POINTS_PAGE_MAX: i64 = 20_000;
pub const CHANGES_PAGE_DEFAULT: i64 = 1_000;
pub const CHANGES_PAGE_MAX: i64 = 5_000;
/// Any local day lies within this many hours of its UTC date: UTC+14 to UTC-12.
const LOCAL_DAY_SLACK_HOURS: i64 = 14;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/days", get(list_days))
        .route("/api/v1/days/{date}", get(get_day))
        .route("/api/v1/points", get(list_points))
        .route("/api/v1/sync/changes", get(sync_changes))
}

// Parameters

type Params = Query<HashMap<String, String>>;

fn param_date(params: &Params, name: &str) -> Result<Option<NaiveDate>, ApiError> {
    params
        .get(name)
        .map(|v| {
            NaiveDate::parse_from_str(v, "%Y-%m-%d")
                .map_err(|_| ApiError::bad_param(name, format!("{name} must be YYYY-MM-DD")))
        })
        .transpose()
}

fn param_time(params: &Params, name: &str) -> Result<Option<DateTime<Utc>>, ApiError> {
    params
        .get(name)
        .map(|v| {
            DateTime::parse_from_rfc3339(v)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|_| ApiError::bad_param(name, format!("{name} must be an RFC 3339 time")))
        })
        .transpose()
}

fn param_limit(params: &Params, default: i64, max: i64) -> Result<i64, ApiError> {
    match params.get("limit") {
        None => Ok(default),
        Some(v) => {
            let n: i64 = v
                .parse()
                .map_err(|_| ApiError::bad_param("limit", "limit must be a whole number"))?;
            if n < 1 || n > max {
                return Err(ApiError::bad_param(
                    "limit",
                    format!("limit must be between 1 and {max}"),
                ));
            }
            Ok(n)
        }
    }
}

fn required<T>(name: &str, value: Option<T>) -> Result<T, ApiError> {
    value.ok_or_else(|| ApiError::bad_param(name, format!("{name} is required")))
}

/// The UTC window that contains every instant of the local days `from..=to` in any zone.
fn utc_window(from: NaiveDate, to: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let slack = Duration::hours(LOCAL_DAY_SLACK_HOURS);
    let lo = Utc.from_utc_datetime(&from.and_hms_opt(0, 0, 0).unwrap()) - slack;
    let hi = Utc.from_utc_datetime(&(to + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap()) + slack;
    (lo, hi)
}

fn zone(tz: &str) -> Tz {
    tz.parse().unwrap_or(chrono_tz::UTC)
}

/// The local calendar day of an instant in its own zone.
pub fn local_date(at: DateTime<Utc>, tz: &str) -> NaiveDate {
    at.with_timezone(&zone(tz)).date_naive()
}

/// Local midnight of a date in a zone; on a day without one, the first instant after it.
pub fn local_midnight(date: NaiveDate, tz: &Tz) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).unwrap();
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(naive + Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

// Cursors

fn encode_cursor(parts: &[&str]) -> String {
    URL_SAFE_NO_PAD.encode(parts.join("|"))
}

fn decode_cursor(cursor: &str, parts: usize) -> Result<Vec<String>, ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| ApiError::bad_param("cursor", "cursor does not decode"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::bad_param("cursor", "cursor does not decode"))?;
    let fields: Vec<String> = text.split('|').map(str::to_string).collect();
    if fields.len() != parts {
        return Err(ApiError::bad_param("cursor", "cursor does not decode"));
    }
    Ok(fields)
}

fn cursor_time(text: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| ApiError::bad_param("cursor", "cursor does not decode"))
}

fn cursor_uuid(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::bad_param("cursor", "cursor does not decode"))
}

fn time_key(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

// Rows

#[derive(Serialize, sqlx::FromRow, Clone, Debug)]
pub struct Segment {
    pub id: Uuid,
    pub device_id: Option<Uuid>,
    pub kind: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub tz: String,
    pub mode_detected: String,
    pub mode_conf: String,
    pub mode_type: String,
    pub distance_m: i32,
    pub point_count: i32,
    pub center_lat: Option<i32>,
    pub center_lon: Option<i32>,
    pub place_id: Option<Uuid>,
    pub source: String,
    pub computed_by: String,
    pub algorithm_version: i16,
    pub steps: Option<i32>,
    pub coarse: bool,
    pub updated_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const SEGMENT_COLUMNS: &str = "id, device_id, kind, start_at, end_at, tz, mode_detected, mode_conf, mode_type, \
    distance_m, point_count, center_lat, center_lon, place_id, source, computed_by, algorithm_version, steps, \
    coarse, updated_at, synced_at, deleted_at";

#[derive(Serialize, sqlx::FromRow, Debug)]
pub struct Correction {
    pub id: Uuid,
    pub kind: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const CORRECTION_COLUMNS: &str =
    "id, kind, start_at, end_at, payload, created_at, updated_at, synced_at, deleted_at";

#[derive(Serialize, sqlx::FromRow, Debug)]
pub struct Place {
    pub id: Uuid,
    pub name: String,
    pub lat: i32,
    pub lon: i32,
    pub radius_m: i16,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const PLACE_COLUMNS: &str =
    "id, name, lat, lon, radius_m, created_at, updated_at, synced_at, deleted_at";

#[derive(Serialize, sqlx::FromRow, Debug)]
pub struct Marker {
    pub id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub tz: String,
    pub lat: i32,
    pub lon: i32,
    pub note: String,
    pub photo_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub synced_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

const MARKER_COLUMNS: &str =
    "id, recorded_at, tz, lat, lon, note, photo_ref, created_at, updated_at, synced_at, deleted_at";

/// A raw point, every column but the owner's.
#[derive(Serialize, sqlx::FromRow, Debug)]
pub struct Point {
    pub device_id: Uuid,
    pub recorded_at: DateTime<Utc>,
    pub tz: String,
    pub lat: i32,
    pub lon: i32,
    pub h_acc_m: i16,
    pub alt_m: Option<i16>,
    pub v_acc_m: Option<i16>,
    pub speed_cms: i16,
    pub course_deg: i16,
    pub activity: i16,
    pub activity_conf: i16,
    pub source: String,
    pub battery_pct: Option<i16>,
    pub battery_state: Option<i16>,
    pub coarse: bool,
    pub import_batch_id: Option<Uuid>,
}

const POINT_COLUMNS: &str = "device_id, recorded_at, tz, lat, lon, h_acc_m, alt_m, v_acc_m, speed_cms, course_deg, \
    activity, activity_conf, source, battery_pct, battery_state, coarse, import_batch_id";

// GET /days

#[derive(Serialize, Debug, PartialEq, Default, Clone)]
pub struct DaySummary {
    pub date: NaiveDate,
    pub point_count: i64,
    pub trip_count: i64,
    pub stop_count: i64,
    pub distance_m: i64,
    pub coarse: bool,
}

/// The local days a segment touches, with the share of its distance on each: in proportion
/// to the time on each side of midnight, whole metres, the remainder on the last day so the
/// parts add up to the whole.
pub fn segment_days(segment: &Segment) -> Vec<(NaiveDate, i64)> {
    let tz = zone(&segment.tz);
    let first = local_date(segment.start_at, &segment.tz);
    let last = local_date(segment.end_at, &segment.tz);
    let total = (segment.end_at - segment.start_at).num_seconds().max(1) as f64;
    let mut out = Vec::new();
    let mut given = 0i64;
    let mut day = first;
    while day <= last {
        let next = day + Duration::days(1);
        let share = if day == last {
            i64::from(segment.distance_m) - given
        } else {
            let day_end = local_midnight(next, &tz);
            let day_start = if day == first {
                segment.start_at
            } else {
                local_midnight(day, &tz)
            };
            let seconds = (day_end - day_start).num_seconds().max(0) as f64;
            let part = (f64::from(segment.distance_m) * seconds / total).round() as i64;
            given += part;
            part
        };
        out.push((day, share));
        day = next;
    }
    out
}

async fn list_days(
    State(state): State<AppState>,
    user: AuthUser,
    params: Params,
) -> Result<Json<Vec<DaySummary>>, ApiError> {
    let from = required("from", param_date(&params, "from")?)?;
    let to = required("to", param_date(&params, "to")?)?;
    if from > to {
        return Err(ApiError::bad_param("to", "to must not be before from"));
    }
    if (to - from).num_days() >= MAX_RANGE_DAYS {
        return Err(ApiError::bad_param(
            "to",
            format!("the range must be under {MAX_RANGE_DAYS} days"),
        ));
    }
    let (lo, hi) = utc_window(from, to);
    let mut days: HashMap<NaiveDate, DaySummary> = HashMap::new();

    let point_days: Vec<(NaiveDate, i64, bool)> = sqlx::query_as(
        "select (recorded_at at time zone tz)::date as day, count(*), bool_or(coarse)
         from points where user_id = $1 and recorded_at >= $2 and recorded_at < $3
         group by 1 having (recorded_at at time zone tz)::date between $4 and $5",
    )
    .bind(user.user_id)
    .bind(lo)
    .bind(hi)
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    for (day, count, coarse) in point_days {
        let entry = days.entry(day).or_insert_with(|| DaySummary {
            date: day,
            ..Default::default()
        });
        entry.point_count = count;
        entry.coarse |= coarse;
    }

    let segments: Vec<Segment> = sqlx::query_as(&format!(
        "select {SEGMENT_COLUMNS} from segments
         where user_id = $1 and computed_by = 'phone' and deleted_at is null and start_at < $3 and end_at > $2"
    ))
    .bind(user.user_id)
    .bind(lo)
    .bind(hi)
    .fetch_all(&state.pool)
    .await?;
    for segment in &segments {
        for (day, distance) in segment_days(segment) {
            if day < from || day > to {
                continue;
            }
            let entry = days.entry(day).or_insert_with(|| DaySummary {
                date: day,
                ..Default::default()
            });
            if segment.kind == "trip" {
                entry.trip_count += 1;
                entry.distance_m += distance;
            } else {
                entry.stop_count += 1;
            }
            entry.coarse |= segment.coarse;
        }
    }
    let mut out: Vec<DaySummary> = days.into_values().collect();
    out.sort_by_key(|d| d.date);
    Ok(Json(out))
}

// GET /days/{date}

#[derive(Serialize, Debug)]
pub struct DayPoint {
    pub recorded_at: DateTime<Utc>,
    pub tz: String,
    pub lat: i32,
    pub lon: i32,
    pub h_acc_m: i16,
    pub speed_cms: i16,
    pub activity: i16,
    pub coarse: bool,
    pub segment_id: Option<Uuid>,
}

#[derive(Serialize, Debug)]
pub struct DayDetail {
    pub date: NaiveDate,
    pub computed_by: String,
    pub segments: Vec<Segment>,
    pub points: Vec<DayPoint>,
}

/// The day's points grouped by the segment that holds each, reduced group by group.
pub fn day_points(points: Vec<Point>, segments: &[Segment], tolerance_m: f64) -> Vec<DayPoint> {
    let mut groups: Vec<(Option<Uuid>, Vec<Point>)> = Vec::new();
    for point in points {
        let holder = segments
            .iter()
            .find(|s| s.start_at <= point.recorded_at && point.recorded_at <= s.end_at)
            .map(|s| s.id);
        match groups.last_mut() {
            Some((id, group)) if *id == holder => group.push(point),
            _ => groups.push((holder, vec![point])),
        }
    }
    let mut out = Vec::new();
    for (segment_id, group) in groups {
        let geos: Vec<Geo> = group
            .iter()
            .map(|p| Geo::from_scaled(p.lat, p.lon))
            .collect();
        for index in simplify(&geos, tolerance_m) {
            let p = &group[index];
            out.push(DayPoint {
                recorded_at: p.recorded_at,
                tz: p.tz.clone(),
                lat: p.lat,
                lon: p.lon,
                h_acc_m: p.h_acc_m,
                speed_cms: p.speed_cms,
                activity: p.activity,
                coarse: p.coarse,
                segment_id,
            });
        }
    }
    out
}

async fn get_day(
    State(state): State<AppState>,
    user: AuthUser,
    Path(date): Path<String>,
    params: Params,
) -> Result<Json<DayDetail>, ApiError> {
    let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
        .map_err(|_| ApiError::bad_param("date", "date must be YYYY-MM-DD"))?;
    let computed_by = params
        .get("computed_by")
        .cloned()
        .unwrap_or_else(|| "phone".to_string());
    if computed_by != "phone" && computed_by != "server" {
        return Err(ApiError::bad_param(
            "computed_by",
            "computed_by must be phone or server",
        ));
    }
    let tolerance_m = match params.get("tolerance_m") {
        None => DEFAULT_TOLERANCE_M,
        Some(v) => {
            let n: f64 = v
                .parse()
                .map_err(|_| ApiError::bad_param("tolerance_m", "tolerance_m must be a number"))?;
            if !(1.0..=500.0).contains(&n) {
                return Err(ApiError::bad_param(
                    "tolerance_m",
                    "tolerance_m must be between 1 and 500",
                ));
            }
            n
        }
    };
    let (lo, hi) = utc_window(date, date);
    let mut segments: Vec<Segment> = sqlx::query_as(&format!(
        "select {SEGMENT_COLUMNS} from segments
         where user_id = $1 and computed_by = $2 and deleted_at is null and start_at < $4 and end_at > $3
         order by start_at"
    ))
    .bind(user.user_id)
    .bind(&computed_by)
    .bind(lo)
    .bind(hi)
    .fetch_all(&state.pool)
    .await?;
    segments.retain(|s| segment_days(s).iter().any(|(day, _)| *day == date));
    if computed_by == "server"
        && let Some(newest) = segments.iter().map(|s| s.algorithm_version).max()
    {
        segments.retain(|s| s.algorithm_version == newest);
    }

    let points_lo = segments
        .iter()
        .map(|s| s.start_at)
        .chain([lo])
        .min()
        .unwrap_or(lo);
    let points_hi = segments
        .iter()
        .map(|s| s.end_at + Duration::seconds(1))
        .chain([hi])
        .max()
        .unwrap_or(hi);
    let mut points: Vec<Point> = sqlx::query_as(&format!(
        "select {POINT_COLUMNS} from points
         where user_id = $1 and recorded_at >= $2 and recorded_at < $3 and h_acc_m <= 100
         order by recorded_at, device_id"
    ))
    .bind(user.user_id)
    .bind(points_lo)
    .bind(points_hi)
    .fetch_all(&state.pool)
    .await?;
    points.retain(|p| {
        local_date(p.recorded_at, &p.tz) == date
            || segments
                .iter()
                .any(|s| s.start_at <= p.recorded_at && p.recorded_at <= s.end_at)
    });
    let points = day_points(points, &segments, tolerance_m);
    Ok(Json(DayDetail {
        date,
        computed_by,
        segments,
        points,
    }))
}

// GET /points

#[derive(Serialize, Debug)]
pub struct PointsPage {
    pub points: Vec<Point>,
    pub next_cursor: Option<String>,
}

async fn list_points(
    State(state): State<AppState>,
    user: AuthUser,
    params: Params,
) -> Result<Json<PointsPage>, ApiError> {
    let from = required("from", param_time(&params, "from")?)?;
    let to = param_time(&params, "to")?.unwrap_or_else(Utc::now);
    let limit = param_limit(&params, POINTS_PAGE_DEFAULT, POINTS_PAGE_MAX)?;
    let (after_at, after_device) = match params.get("cursor") {
        None => (from - Duration::seconds(1), Uuid::nil()),
        Some(cursor) => {
            let parts = decode_cursor(cursor, 2)?;
            (cursor_time(&parts[0])?, cursor_uuid(&parts[1])?)
        }
    };
    let mut points: Vec<Point> = sqlx::query_as(&format!(
        "select {POINT_COLUMNS} from points
         where user_id = $1 and recorded_at >= $2 and recorded_at < $3 and (recorded_at, device_id) > ($4, $5)
         order by recorded_at, device_id limit $6"
    ))
    .bind(user.user_id)
    .bind(from)
    .bind(to)
    .bind(after_at)
    .bind(after_device)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let next_cursor = if points.len() as i64 > limit {
        points.truncate(limit as usize);
        let last = points.last().expect("a full page");
        Some(encode_cursor(&[
            &time_key(last.recorded_at),
            &last.device_id.to_string(),
        ]))
    } else {
        None
    };
    Ok(Json(PointsPage {
        points,
        next_cursor,
    }))
}

// GET /sync/changes

#[derive(Serialize, Debug)]
pub struct ChangesPage {
    pub segments: Vec<Segment>,
    pub corrections: Vec<Correction>,
    pub places: Vec<Place>,
    pub markers: Vec<Marker>,
    pub next_cursor: Option<String>,
    pub server_time: DateTime<Utc>,
}

/// One row's place in the stream over the four tables.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ChangeKey {
    synced_at: DateTime<Utc>,
    table: &'static str,
    id: Uuid,
}

/// One page's window over the four upsert tables: the caller, where it starts, the cursor
/// it continues from and how many rows it takes.
struct ChangeWindow {
    user_id: Uuid,
    since: DateTime<Utc>,
    after: ChangeKey,
    limit: i64,
}

async fn changed_rows<T>(
    state: &AppState,
    table: &'static str,
    columns: &str,
    extra: &str,
    window: &ChangeWindow,
) -> Result<Vec<T>, ApiError>
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    let sql = format!(
        "select {columns} from {table}
         where user_id = $1 and synced_at > $2 and (synced_at, '{table}'::text, id) > ($3, $4, $5) {extra}
         order by synced_at, id limit $6"
    );
    Ok(sqlx::query_as(&sql)
        .bind(window.user_id)
        .bind(window.since)
        .bind(window.after.synced_at)
        .bind(window.after.table)
        .bind(window.after.id)
        .bind(window.limit)
        .fetch_all(&state.pool)
        .await?)
}

async fn sync_changes(
    State(state): State<AppState>,
    user: AuthUser,
    params: Params,
) -> Result<Json<ChangesPage>, ApiError> {
    let since = required("since", param_time(&params, "since")?)?;
    let limit = param_limit(&params, CHANGES_PAGE_DEFAULT, CHANGES_PAGE_MAX)?;
    let after = match params.get("cursor") {
        None => ChangeKey {
            synced_at: since,
            table: "",
            id: Uuid::nil(),
        },
        Some(cursor) => {
            let parts = decode_cursor(cursor, 3)?;
            ChangeKey {
                synced_at: cursor_time(&parts[0])?,
                table: match parts[1].as_str() {
                    "corrections" => "corrections",
                    "markers" => "markers",
                    "places" => "places",
                    "segments" => "segments",
                    _ => return Err(ApiError::bad_param("cursor", "cursor does not decode")),
                },
                id: cursor_uuid(&parts[2])?,
            }
        }
    };
    let window = ChangeWindow {
        user_id: user.user_id,
        since,
        after,
        limit,
    };
    let server_time: DateTime<Utc> = sqlx::query_scalar("select now()")
        .fetch_one(&state.pool)
        .await?;
    let segments: Vec<Segment> = changed_rows(
        &state,
        "segments",
        SEGMENT_COLUMNS,
        "and computed_by <> 'server'",
        &window,
    )
    .await?;
    let corrections: Vec<Correction> =
        changed_rows(&state, "corrections", CORRECTION_COLUMNS, "", &window).await?;
    let places: Vec<Place> = changed_rows(&state, "places", PLACE_COLUMNS, "", &window).await?;
    let markers: Vec<Marker> = changed_rows(&state, "markers", MARKER_COLUMNS, "", &window).await?;

    // Merge the four streams by key and keep the first `limit` rows of the merged order.
    let full = [
        segments.len(),
        corrections.len(),
        places.len(),
        markers.len(),
    ]
    .iter()
    .any(|n| *n as i64 >= limit);
    let mut keys: Vec<ChangeKey> = segments
        .iter()
        .map(|r| ChangeKey {
            synced_at: r.synced_at,
            table: "segments",
            id: r.id,
        })
        .chain(corrections.iter().map(|r| ChangeKey {
            synced_at: r.synced_at,
            table: "corrections",
            id: r.id,
        }))
        .chain(places.iter().map(|r| ChangeKey {
            synced_at: r.synced_at,
            table: "places",
            id: r.id,
        }))
        .chain(markers.iter().map(|r| ChangeKey {
            synced_at: r.synced_at,
            table: "markers",
            id: r.id,
        }))
        .collect();
    keys.sort();
    let more = keys.len() as i64 > limit || full;
    keys.truncate(limit as usize);
    let next_cursor = if more {
        keys.last()
            .map(|k| encode_cursor(&[&time_key(k.synced_at), k.table, &k.id.to_string()]))
    } else {
        None
    };
    let last = keys.last().cloned();
    let within = |synced_at: DateTime<Utc>, table: &'static str, id: Uuid| {
        last.as_ref().is_some_and(|l| {
            ChangeKey {
                synced_at,
                table,
                id,
            } <= *l
        })
    };
    Ok(Json(ChangesPage {
        segments: segments
            .into_iter()
            .filter(|r| within(r.synced_at, "segments", r.id))
            .collect(),
        corrections: corrections
            .into_iter()
            .filter(|r| within(r.synced_at, "corrections", r.id))
            .collect(),
        places: places
            .into_iter()
            .filter(|r| within(r.synced_at, "places", r.id))
            .collect(),
        markers: markers
            .into_iter()
            .filter(|r| within(r.synced_at, "markers", r.id))
            .collect(),
        next_cursor,
        server_time,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start: &str, end: &str, distance_m: i32) -> Segment {
        Segment {
            id: Uuid::nil(),
            device_id: None,
            kind: "trip".into(),
            start_at: DateTime::parse_from_rfc3339(start)
                .unwrap()
                .with_timezone(&Utc),
            end_at: DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&Utc),
            tz: "Europe/Warsaw".into(),
            mode_detected: "walking".into(),
            mode_conf: "high".into(),
            mode_type: "walk".into(),
            distance_m,
            point_count: 10,
            center_lat: None,
            center_lon: None,
            place_id: None,
            source: "phone".into(),
            computed_by: "phone".into(),
            algorithm_version: 1,
            steps: None,
            coarse: false,
            updated_at: Utc::now(),
            synced_at: Utc::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn a_trip_over_midnight_splits_its_distance_by_time_and_the_parts_add_up() {
        // 23:20 to 00:20 Warsaw time (UTC+2 in October): 40 minutes before midnight, 20 after.
        let s = segment("2026-10-12T21:20:00Z", "2026-10-12T22:20:00Z", 1_000);
        let days = segment_days(&s);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].0.to_string(), "2026-10-12");
        assert_eq!(days[0].1, 667);
        assert_eq!(days[1].0.to_string(), "2026-10-13");
        assert_eq!(days[1].1, 333);
    }

    #[test]
    fn a_trip_within_a_day_keeps_its_whole_distance() {
        let s = segment("2026-10-12T08:00:00Z", "2026-10-12T09:00:00Z", 4_200);
        assert_eq!(
            segment_days(&s),
            vec![("2026-10-12".parse().unwrap(), 4_200)]
        );
    }

    #[test]
    fn the_night_the_clocks_go_back_is_still_split_by_real_time() {
        // 2026-10-24 23:00 CEST to 2026-10-25 03:00 CET: 1 h before midnight, 4 h after.
        let s = segment("2026-10-24T21:00:00Z", "2026-10-25T02:00:00Z", 500);
        let days = segment_days(&s);
        assert_eq!(days[0].1, 100);
        assert_eq!(days[1].1, 400);
    }

    #[test]
    fn cursors_round_trip_and_garbage_is_refused() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap();
        let id = Uuid::now_v7();
        let cursor = encode_cursor(&[&time_key(at), &id.to_string()]);
        let parts = decode_cursor(&cursor, 2).unwrap();
        assert_eq!(cursor_time(&parts[0]).unwrap(), at);
        assert_eq!(cursor_uuid(&parts[1]).unwrap(), id);
        assert_eq!(
            decode_cursor("not base64!", 2)
                .unwrap_err()
                .field
                .as_deref(),
            Some("cursor")
        );
        assert!(decode_cursor(&cursor, 3).is_err());
    }
}
