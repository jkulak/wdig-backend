//! A made-up month for an account, over real streets: the routes come from Apple Maps
//! directions between real places in Warsaw (`fixtures/demo_routes.json`, written by the app's
//! `scripts/demo-routes.swift`), and the points along them are sampled the way the recorder
//! keeps fixes: 10 m on foot, 25 m on a bike, 50 m in a car or every 60 s, with the first
//! hundred metres from the door missing, GPS noise and a stop's few warm-phase fixes. The
//! app's demo month reads the same file, so a phone and a server tell the same story.
//! Deterministic for a given seed; `--replace` removes the previous run.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use clap::Args;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::partitions;

const ROUTES_JSON: &str = include_str!("../fixtures/demo_routes.json");
const SEED_DEVICE_NAME: &str = "seed";
const SCALE: f64 = 10_000_000.0;
const DOOR_GAP_M: f64 = 120.0;

#[derive(Args, Debug)]
pub struct SeedArgs {
    /// The account; created with an unusable password when it does not exist
    #[arg(long)]
    pub email: String,
    /// How many days back from today
    #[arg(long, default_value_t = 30)]
    pub days: u32,
    /// The IANA time zone the days are in
    #[arg(long, default_value = "Europe/Warsaw")]
    pub tz: String,
    /// Remove what an earlier seed wrote for this account first
    #[arg(long)]
    pub replace: bool,
    /// The random seed, for a different month
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
}

#[derive(Deserialize)]
struct Routes {
    places: HashMap<String, PlaceSpec>,
    legs: HashMap<String, LegSpec>,
}

#[derive(Deserialize)]
struct PlaceSpec {
    lat: f64,
    lon: f64,
}

#[derive(Deserialize)]
struct LegSpec {
    /// Apple Maps' length of the leg; the tests check the sampled length against it.
    #[cfg_attr(not(test), allow(dead_code))]
    #[serde(rename = "distanceM")]
    distance_m: i64,
    coordinates: Vec<[f64; 2]>,
}

impl Routes {
    fn load() -> Result<Routes> {
        serde_json::from_str(ROUTES_JSON).context("fixtures/demo_routes.json does not parse")
    }

    fn place(&self, key: &str) -> Geo {
        self.places
            .get(key)
            .map(|p| Geo {
                lat: p.lat,
                lon: p.lon,
            })
            .unwrap_or(Geo {
                lat: 52.2288,
                lon: 21.0168,
            })
    }

    fn leg(&self, from: &str, to: &str, mode: &str) -> Option<&LegSpec> {
        self.legs.get(&format!("{from}-{to}-{mode}"))
    }
}

#[derive(Clone, Copy, Debug)]
struct Geo {
    lat: f64,
    lon: f64,
}

impl Geo {
    fn offset(self, north_m: f64, east_m: f64) -> Geo {
        Geo {
            lat: self.lat + north_m / 111_320.0,
            lon: self.lon + east_m / (111_320.0 * self.lat.to_radians().cos()),
        }
    }

    fn distance(self, other: Geo) -> f64 {
        let radius = 6_371_008.8;
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let d_lat = lat2 - lat1;
        let d_lon = (other.lon - self.lon).to_radians();
        let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
        2.0 * radius * a.sqrt().min(1.0).asin()
    }

    fn bearing(self, other: Geo) -> f64 {
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let d_lon = (other.lon - self.lon).to_radians();
        let y = d_lon.sin() * lat2.cos();
        let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * d_lon.cos();
        (y.atan2(x).to_degrees() + 360.0) % 360.0
    }

    fn scaled(self) -> (i32, i32) {
        (
            (self.lat * SCALE).round() as i32,
            (self.lon * SCALE).round() as i32,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Walk,
    Run,
    Bike,
    Car,
}

impl Mode {
    fn speed_mps(self) -> f64 {
        match self {
            Mode::Walk => 1.4,
            Mode::Run => 2.9,
            Mode::Bike => 5.5,
            Mode::Car => 9.0,
        }
    }

    fn spacing_m(self) -> f64 {
        match self {
            Mode::Walk | Mode::Run => 10.0,
            Mode::Bike => 25.0,
            Mode::Car => 50.0,
        }
    }

    fn noise_m(self) -> f64 {
        if self == Mode::Car { 6.0 } else { 3.0 }
    }

    /// `activity` class code: 2 walking, 3 running, 4 cycling, 5 automotive.
    fn activity(self) -> i16 {
        match self {
            Mode::Walk => 2,
            Mode::Run => 3,
            Mode::Bike => 4,
            Mode::Car => 5,
        }
    }

    fn detected(self) -> &'static str {
        match self {
            Mode::Walk => "walking",
            Mode::Run => "running",
            Mode::Bike => "cycling",
            Mode::Car => "automotive",
        }
    }

    fn mode_type(self) -> &'static str {
        match self {
            Mode::Walk => "walk",
            Mode::Run => "run",
            Mode::Bike => "bike",
            Mode::Car => "car",
        }
    }

    /// The leg to route on: runs follow the walking routes.
    fn leg_mode(self) -> &'static str {
        if self == Mode::Run {
            "walk"
        } else {
            self.mode_type()
        }
    }
}

struct Point {
    at: DateTime<Utc>,
    geo: Geo,
    h_acc: i16,
    speed_cms: i16,
    course: i16,
    activity: i16,
    activity_conf: i16,
}

struct Segment {
    kind: &'static str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    mode: Option<Mode>,
    distance_m: i32,
    point_count: i32,
    center: Option<Geo>,
    steps: Option<i32>,
}

struct Day {
    points: Vec<Point>,
    segments: Vec<Segment>,
}

struct Visit {
    place: &'static str,
    mode: Mode,
    stay_s: i64,
}

/// Where the day goes, in order, by the weekday and the dice; the day starts and ends at home.
fn plan(rng: &mut StdRng, weekend: bool) -> (i64, Vec<Visit>) {
    let mut visits = Vec::new();
    let visit = |place, mode, stay_s| Visit {
        place,
        mode,
        stay_s,
    };
    if weekend {
        let roll: f64 = rng.random_range(0.0..1.0);
        if roll < 0.3 {
            visits.push(visit("vistula", Mode::Bike, 5 * 60));
            visits.push(visit("lomianki", Mode::Bike, 45 * 60));
            visits.push(visit("vistula", Mode::Bike, 10 * 60));
            visits.push(visit("home", Mode::Bike, 0));
        } else if roll < 0.55 {
            visits.push(visit("oldTown", Mode::Walk, 90 * 60));
            visits.push(visit("home", Mode::Walk, 0));
        } else if roll < 0.75 {
            visits.push(visit("zloteTarasy", Mode::Walk, 100 * 60));
            visits.push(visit("home", Mode::Walk, 0));
        } else if roll < 0.9 {
            visits.push(visit("lazienki", Mode::Run, 4 * 60));
            visits.push(visit("home", Mode::Run, 0));
        } else {
            visits.push(visit("vistula", Mode::Walk, 40 * 60));
            visits.push(visit("home", Mode::Walk, 0));
        }
        visits.push(visit("home", Mode::Walk, rng.random_range(60..120) * 60));
        if rng.random_range(0.0..1.0) < 0.4 {
            if rng.random_range(0.0..1.0) < 0.5 {
                visits.push(visit("kinoteka", Mode::Walk, 150 * 60));
            } else {
                visits.push(visit("koszyki", Mode::Walk, 60 * 60));
            }
            visits.push(visit("home", Mode::Walk, 0));
        }
        return (10 * 3600 + rng.random_range(-30..30) * 60, visits);
    }
    visits.push(visit(
        "office",
        Mode::Car,
        (8 * 60 + 20 + rng.random_range(-30..30)) * 60,
    ));
    visits.push(visit("home", Mode::Car, rng.random_range(45..90) * 60));
    let roll: f64 = rng.random_range(0.0..1.0);
    if roll < 0.3 {
        visits.push(visit("vistula", Mode::Walk, 25 * 60));
        visits.push(visit("home", Mode::Walk, 0));
    } else if roll < 0.5 {
        visits.push(visit("lazienki", Mode::Run, 4 * 60));
        visits.push(visit("home", Mode::Run, 0));
    } else if roll < 0.65 {
        visits.push(visit("kinoteka", Mode::Walk, 150 * 60));
        visits.push(visit("home", Mode::Walk, 0));
    } else if roll < 0.8 {
        visits.push(visit("koszyki", Mode::Walk, 50 * 60));
        visits.push(visit("home", Mode::Walk, 0));
    }
    (7 * 3600 + 50 * 60 + rng.random_range(-15..15) * 60, visits)
}

/// A stay: a handful of fixes in the first minutes (the warm phase), then nothing.
fn stay(
    rng: &mut StdRng,
    at: Geo,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> (Vec<Point>, Segment) {
    let mut points = Vec::new();
    let count = rng.random_range(3..7);
    for index in 0..count {
        let geo = at.offset(rng.random_range(-20.0..20.0), rng.random_range(-20.0..20.0));
        points.push(Point {
            at: start + Duration::seconds(30 + index * 100),
            geo,
            h_acc: rng.random_range(20..80),
            speed_cms: 0,
            course: -1,
            activity: 1,
            activity_conf: 2,
        });
    }
    let segment = Segment {
        kind: "stop",
        start,
        end,
        mode: None,
        distance_m: 0,
        point_count: count as i32,
        center: Some(at),
        steps: None,
    };
    (points, segment)
}

/// Walks the leg's polyline at the mode's speed, keeping a fix every `spacing` metres or 60 s
/// as the recorder would, after the first `DOOR_GAP_M` metres the recorder misses.
fn sample(
    rng: &mut StdRng,
    leg: &LegSpec,
    mode: Mode,
    start: DateTime<Utc>,
) -> (Vec<Point>, Segment) {
    let coordinates: Vec<Geo> = leg
        .coordinates
        .iter()
        .map(|c| Geo {
            lat: c[0],
            lon: c[1],
        })
        .collect();
    let mut points: Vec<Point> = Vec::new();
    let mut time = start;
    let mut travelled = 0.0;
    let mut since_last_kept = -DOOR_GAP_M;
    let mut last_kept_at = start;
    let mut speed = mode.speed_mps();
    let mut stretch_left = 0.0;
    let mut recording = false;
    let keep = |rng: &mut StdRng,
                points: &mut Vec<Point>,
                spot: Geo,
                time: DateTime<Utc>,
                speed: f64,
                course: f64| {
        let noise = mode.noise_m();
        let noisy = spot.offset(
            rng.random_range(-noise..noise),
            rng.random_range(-noise..noise),
        );
        points.push(Point {
            at: time,
            geo: noisy,
            h_acc: rng.random_range(4..16),
            speed_cms: (speed * 100.0).round() as i16,
            course: (course.round() as i16).rem_euclid(360),
            activity: mode.activity(),
            activity_conf: if rng.random_range(0.0..1.0) < 0.85 {
                2
            } else {
                1
            },
        });
    };
    for index in 1..coordinates.len() {
        let from = coordinates[index - 1];
        let to = coordinates[index];
        let length = from.distance(to);
        let course = from.bearing(to);
        let mut along = 0.0;
        while along < length {
            if stretch_left <= 0.0 {
                speed = mode.speed_mps() * rng.random_range(0.8..1.2);
                stretch_left = rng.random_range(150.0..300.0);
            }
            let step = (mode.spacing_m() - since_last_kept.max(0.0))
                .min(stretch_left)
                .min(length - along);
            along += step;
            travelled += step;
            stretch_left -= step;
            since_last_kept += step;
            time += Duration::milliseconds((step / speed * 1000.0) as i64);
            let spot = from.offset(
                along * course.to_radians().cos(),
                along * course.to_radians().sin(),
            );
            if !recording && since_last_kept >= 0.0 {
                // The door gap is over: the recorder has its first fix from here on.
                recording = true;
                last_kept_at = time;
            }
            if recording
                && (since_last_kept >= mode.spacing_m()
                    || (time - last_kept_at).num_seconds() >= 60)
            {
                keep(rng, &mut points, spot, time, speed, course);
                last_kept_at = time;
                since_last_kept = 0.0;
            }
        }
        if mode == Mode::Car && index < coordinates.len() - 1 && rng.random_range(0.0..1.0) < 0.12 {
            let pause = rng.random_range(15.0..75.0);
            time += Duration::milliseconds((pause * 1000.0) as i64);
            if pause >= 60.0 {
                keep(rng, &mut points, to, time, 0.0, course);
                last_kept_at = time;
                since_last_kept = 0.0;
            }
        }
    }
    if let Some(last) = coordinates.last()
        && since_last_kept > 0.0
    {
        let course = points.last().map(|p| p.course as f64).unwrap_or(0.0);
        keep(rng, &mut points, *last, time, speed, course);
    }
    let steps = match mode {
        Mode::Walk => Some((travelled / 0.75) as i32),
        Mode::Run => Some((travelled / 1.1) as i32),
        _ => None,
    };
    let segment = Segment {
        kind: "trip",
        start,
        end: time,
        mode: Some(mode),
        distance_m: travelled.round() as i32,
        point_count: points.len() as i32,
        center: None,
        steps,
    };
    (points, segment)
}

/// One local day, from midnight to midnight, in the day's zone.
fn day(rng: &mut StdRng, routes: &Routes, date: NaiveDate, tz: Tz) -> Day {
    let midnight = tz
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc);
    let next_midnight = tz
        .from_local_datetime(&(date + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc);
    let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);
    let (departure_s, visits) = plan(rng, weekend);
    let mut points = Vec::new();
    let mut segments = Vec::new();
    let mut now = midnight + Duration::seconds(departure_s);
    let mut here = "home";
    let (stay_points, stay_segment) = stay(rng, routes.place("home"), midnight, now);
    points.extend(stay_points);
    segments.push(stay_segment);
    for visit in visits {
        if visit.place != here {
            let Some(leg) = routes.leg(here, visit.place, visit.mode.leg_mode()) else {
                continue;
            };
            let (trip_points, trip_segment) = sample(rng, leg, visit.mode, now);
            now = trip_segment.end;
            points.extend(trip_points);
            segments.push(trip_segment);
            here = visit.place;
        }
        if visit.stay_s > 0 {
            let leave = now + Duration::seconds(visit.stay_s);
            let (stay_points, stay_segment) = stay(rng, routes.place(here), now, leave);
            points.extend(stay_points);
            segments.push(stay_segment);
            now = leave;
        }
    }
    let (stay_points, stay_segment) = stay(rng, routes.place("home"), now, next_midnight);
    points.extend(stay_points);
    segments.push(stay_segment);
    Day { points, segments }
}

pub async fn run(pool: &PgPool, args: SeedArgs) -> Result<()> {
    let tz: Tz = args
        .tz
        .parse()
        .map_err(|_| anyhow::anyhow!("{} is not an IANA time zone", args.tz))?;
    if args.days == 0 || args.days > 400 {
        bail!("--days takes 1 to 400");
    }
    let routes = Routes::load()?;
    let mut rng = StdRng::seed_from_u64(args.seed);

    let user_id: Uuid = match sqlx::query_scalar("select id from users where email = $1::citext")
        .bind(&args.email)
        .fetch_optional(pool)
        .await?
    {
        Some(id) => id,
        None => sqlx::query_scalar(
            "insert into users (email, password_hash) values ($1, '!seed') returning id",
        )
        .bind(&args.email)
        .fetch_one(pool)
        .await
        .context("creating the account")?,
    };
    let device_id: Uuid = match sqlx::query_scalar(
        "select id from devices where user_id = $1 and name = $2",
    )
    .bind(user_id)
    .bind(SEED_DEVICE_NAME)
    .fetch_optional(pool)
    .await?
    {
        Some(id) => id,
        None => {
            let id = Uuid::now_v7();
            sqlx::query("insert into devices (id, user_id, name, platform, app_version) values ($1, $2, $3, 'ios', 'seed')")
                .bind(id)
                .bind(user_id)
                .bind(SEED_DEVICE_NAME)
                .execute(pool)
                .await?;
            id
        }
    };
    if args.replace {
        sqlx::query("delete from segments where device_id = $1")
            .bind(device_id)
            .execute(pool)
            .await?;
        sqlx::query("delete from points where device_id = $1")
            .bind(device_id)
            .execute(pool)
            .await?;
    }

    let today = Utc::now().with_timezone(&tz).date_naive();
    let first = today - Duration::days(args.days as i64);
    partitions::ensure(pool, first, (args.days / 28) + 2, false).await?;

    let mut total_points = 0usize;
    let mut total_segments = 0usize;
    for offset in 0..args.days as i64 {
        let date = first + Duration::days(offset);
        let generated = day(&mut rng, &routes, date, tz);
        total_points += generated.points.len();
        total_segments += generated.segments.len();
        insert_day(pool, user_id, device_id, &args.tz, &generated)
            .await
            .with_context(|| format!("day {date}"))?;
        eprintln!(
            "day {} of {}: {date}, {} points, {} segments",
            offset + 1,
            args.days,
            generated.points.len(),
            generated.segments.len()
        );
    }
    println!(
        "ok {} days, {total_points} points, {total_segments} segments for {} (device {device_id})",
        args.days, args.email
    );
    Ok(())
}

async fn insert_day(
    pool: &PgPool,
    user_id: Uuid,
    device_id: Uuid,
    tz: &str,
    day: &Day,
) -> Result<()> {
    let n = day.points.len();
    let mut recorded_at = Vec::with_capacity(n);
    let mut lats = Vec::with_capacity(n);
    let mut lons = Vec::with_capacity(n);
    let mut h_acc = Vec::with_capacity(n);
    let mut speeds = Vec::with_capacity(n);
    let mut courses = Vec::with_capacity(n);
    let mut activities = Vec::with_capacity(n);
    let mut confs = Vec::with_capacity(n);
    let mut battery = Vec::with_capacity(n);
    let day_start = day.points.first().map(|p| p.at).unwrap_or_else(Utc::now);
    for point in &day.points {
        let (lat, lon) = point.geo.scaled();
        recorded_at.push(point.at);
        lats.push(lat);
        lons.push(lon);
        h_acc.push(point.h_acc);
        speeds.push(point.speed_cms);
        courses.push(point.course);
        activities.push(point.activity);
        confs.push(point.activity_conf);
        let hours = (point.at - day_start).num_minutes() as f64 / 60.0;
        battery.push((100.0 - hours * 3.5).max(15.0) as i16);
    }
    sqlx::query(
        "insert into points (user_id, device_id, recorded_at, tz, lat, lon, h_acc_m, speed_cms, course_deg, activity, activity_conf, source, battery_pct, battery_state)
         select $1, $2, r, $3, la, lo, h, s, c, a, ac, 'phone', b, 1
         from unnest($4::timestamptz[], $5::int4[], $6::int4[], $7::int2[], $8::int2[], $9::int2[], $10::int2[], $11::int2[], $12::int2[]) as t(r, la, lo, h, s, c, a, ac, b)
         on conflict (device_id, recorded_at) do nothing",
    )
    .bind(user_id)
    .bind(device_id)
    .bind(tz)
    .bind(&recorded_at)
    .bind(&lats)
    .bind(&lons)
    .bind(&h_acc)
    .bind(&speeds)
    .bind(&courses)
    .bind(&activities)
    .bind(&confs)
    .bind(&battery)
    .execute(pool)
    .await?;

    for segment in &day.segments {
        let (center_lat, center_lon) = match segment.center {
            Some(center) => {
                let (lat, lon) = center.scaled();
                (Some(lat), Some(lon))
            }
            None => (None, None),
        };
        let (detected, conf, mode_type) = match segment.mode {
            Some(mode) => (mode.detected(), "high", mode.mode_type()),
            None => ("stationary", "high", "other"),
        };
        sqlx::query(
            "insert into segments (id, user_id, device_id, kind, start_at, end_at, tz, mode_detected, mode_conf, mode_type, distance_m, point_count, center_lat, center_lon, source, computed_by, algorithm_version, steps)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, 'phone', 'phone', 1, $15)",
        )
        .bind(Uuid::now_v7())
        .bind(user_id)
        .bind(device_id)
        .bind(segment.kind)
        .bind(segment.start)
        .bind(segment.end)
        .bind(tz)
        .bind(detected)
        .bind(conf)
        .bind(mode_type)
        .bind(segment.distance_m)
        .bind(segment.point_count)
        .bind(center_lat)
        .bind(center_lon)
        .bind(segment.steps)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_fixture_has_every_leg_the_plans_ask_for() {
        let routes = Routes::load().unwrap();
        for (from, to, mode) in [
            ("home", "oldTown", "walk"),
            ("home", "vistula", "walk"),
            ("home", "zloteTarasy", "walk"),
            ("home", "kinoteka", "walk"),
            ("home", "lazienki", "walk"),
            ("home", "koszyki", "walk"),
            ("home", "vistula", "bike"),
            ("vistula", "lomianki", "bike"),
            ("home", "office", "car"),
        ] {
            assert!(routes.leg(from, to, mode).is_some(), "{from}-{to}-{mode}");
            assert!(routes.leg(to, from, mode).is_some(), "{to}-{from}-{mode}");
        }
    }

    #[test]
    fn a_walk_to_the_old_town_is_sampled_every_ten_metres_after_the_door_gap() {
        let routes = Routes::load().unwrap();
        let leg = routes.leg("home", "oldTown", "walk").unwrap();
        let mut rng = StdRng::seed_from_u64(3);
        let start = Utc::now();
        let (points, segment) = sample(&mut rng, leg, Mode::Walk, start);
        let minutes = (segment.end - start).num_minutes();
        assert!(
            (25..=45).contains(&minutes),
            "{minutes} minutes for {} m",
            segment.distance_m
        );
        assert!((segment.distance_m as i64 - leg.distance_m).abs() < leg.distance_m / 10);
        assert!(
            points.len() as i64 > leg.distance_m / 14 && (points.len() as i64) < leg.distance_m / 8,
            "{} points",
            points.len()
        );
        assert!((points[0].at - start).num_seconds() > 60);
        assert!(routes.place("home").distance(points[0].geo) > 80.0);
        assert!(
            points
                .iter()
                .all(|p| p.activity == 2 && p.speed_cms > 90 && p.speed_cms < 200)
        );
    }

    #[test]
    fn a_weekday_goes_to_the_office_by_car_and_ends_at_home() {
        let routes = Routes::load().unwrap();
        let mut rng = StdRng::seed_from_u64(1);
        let generated = day(
            &mut rng,
            &routes,
            NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            chrono_tz::Europe::Warsaw,
        );
        assert_eq!(generated.segments.first().map(|s| s.kind), Some("stop"));
        assert_eq!(generated.segments.last().map(|s| s.kind), Some("stop"));
        assert!(
            generated
                .segments
                .iter()
                .any(|s| s.kind == "trip" && s.mode == Some(Mode::Car))
        );
        assert!(
            generated
                .segments
                .iter()
                .any(|s| s.kind == "stop" && (s.end - s.start).num_hours() >= 7)
        );
        for pair in generated.segments.windows(2) {
            assert!(pair[0].end <= pair[1].start + Duration::seconds(1));
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_day() {
        let routes = Routes::load().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let a = day(
            &mut StdRng::seed_from_u64(5),
            &routes,
            date,
            chrono_tz::Europe::Warsaw,
        );
        let b = day(
            &mut StdRng::seed_from_u64(5),
            &routes,
            date,
            chrono_tz::Europe::Warsaw,
        );
        assert_eq!(a.points.len(), b.points.len());
        assert_eq!(a.points[10].geo.scaled(), b.points[10].geo.scaled());
    }
}
