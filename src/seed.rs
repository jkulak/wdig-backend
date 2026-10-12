//! A made-up month for an account: the days before any phone has recorded, so the app and
//! the API have something to show. The shape is an ordinary life around a home: weekdays to
//! work and back, an evening walk now and then, weekends with a long walk, a ride or a drive.
//! Everything is deterministic for a given seed, and `--replace` removes the previous run.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use clap::Args;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sqlx::PgPool;
use uuid::Uuid;

use crate::partitions;

#[derive(Args, Debug)]
pub struct SeedArgs {
    /// The account; created with an unusable password when it does not exist
    #[arg(long)]
    pub email: String,
    /// How many days back from today
    #[arg(long, default_value_t = 30)]
    pub days: u32,
    /// Home, where every day starts and ends
    #[arg(long, default_value_t = 52.2297)]
    pub lat: f64,
    #[arg(long, default_value_t = 21.0122)]
    pub lon: f64,
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

const SEED_DEVICE_NAME: &str = "seed";
const SCALE: f64 = 10_000_000.0;

#[derive(Clone, Copy, Debug)]
struct Geo {
    lat: f64,
    lon: f64,
}

impl Geo {
    fn offset(self, north_m: f64, east_m: f64) -> Geo {
        let lat = self.lat + north_m / 111_320.0;
        let lon = self.lon + east_m / (111_320.0 * self.lat.to_radians().cos());
        Geo { lat, lon }
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
            Mode::Car => 11.0,
        }
    }

    fn spacing_m(self) -> f64 {
        match self {
            Mode::Walk | Mode::Run => 10.0,
            Mode::Bike => 25.0,
            Mode::Car => 50.0,
        }
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

/// A trip from `from` to `to` by `mode`, as a gently curved line of points at the mode's
/// spacing, with the speed and the course of each point and a little position noise.
fn trip(
    rng: &mut StdRng,
    from: Geo,
    to: Geo,
    mode: Mode,
    start: DateTime<Utc>,
) -> (Vec<Point>, Segment) {
    let straight = from.distance(to).max(50.0);
    // Two waypoints off the straight line make the route look like streets, not a ruler.
    let bend = straight * 0.12;
    let bearing = from.bearing(to).to_radians();
    let side = |geo: Geo, along: f64, off: f64| -> Geo {
        let north = along * bearing.cos() - off * bearing.sin();
        let east = along * bearing.sin() + off * bearing.cos();
        geo.offset(north, east)
    };
    let w1 = side(from, straight * 0.33, rng.random_range(-bend..bend));
    let w2 = side(from, straight * 0.66, rng.random_range(-bend..bend));
    let legs = [(from, w1), (w1, w2), (w2, to)];
    let mut points = Vec::new();
    let mut at = start;
    let mut distance = 0.0;
    let speed = mode.speed_mps();
    for (a, b) in legs {
        let length = a.distance(b);
        let steps = (length / mode.spacing_m()).ceil().max(1.0) as usize;
        for index in 0..steps {
            let fraction = index as f64 / steps as f64;
            let leg_bearing = a.bearing(b).to_radians();
            let along = fraction * length;
            let noise = rng.random_range(-3.0..3.0);
            let geo = a.offset(
                along * leg_bearing.cos() + noise * leg_bearing.sin(),
                along * leg_bearing.sin() - noise * leg_bearing.cos(),
            );
            let variation = rng.random_range(0.75..1.25);
            let point_speed = if mode == Mode::Car && rng.random_range(0.0..1.0) < 0.08 {
                0.0
            } else {
                speed * variation
            };
            points.push(Point {
                at,
                geo,
                h_acc: rng.random_range(4..16),
                speed_cms: (point_speed * 100.0).round() as i16,
                course: a.bearing(b).round() as i16 % 360,
                activity: mode.activity(),
                activity_conf: if rng.random_range(0.0..1.0) < 0.85 {
                    2
                } else {
                    1
                },
            });
            let step_m = length / steps as f64;
            distance += step_m;
            let seconds =
                step_m / point_speed.max(0.5) + if point_speed == 0.0 { 20.0 } else { 0.0 };
            at += Duration::milliseconds((seconds * 1000.0) as i64);
        }
    }
    let end = at;
    let steps = match mode {
        Mode::Walk => Some((distance / 0.75) as i32),
        Mode::Run => Some((distance / 1.1) as i32),
        _ => None,
    };
    let segment = Segment {
        kind: "trip",
        start,
        end,
        mode: Some(mode),
        distance_m: distance.round() as i32,
        point_count: points.len() as i32,
        center: None,
        steps,
    };
    (points, segment)
}

/// A stay: a handful of fixes in the first minutes (the warm phase), then nothing.
fn stop(
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

/// The places of the made-up life, around the home.
struct Places {
    home: Geo,
    work: Geo,
    gym: Geo,
    shop: Geo,
    park: Geo,
    lake: Geo,
}

impl Places {
    fn around(home: Geo) -> Places {
        Places {
            home,
            work: home.offset(2_100.0, 2_400.0),
            gym: home.offset(-900.0, 1_100.0),
            shop: home.offset(500.0, -600.0),
            park: home.offset(1_500.0, -1_200.0),
            lake: home.offset(-18_000.0, 16_000.0),
        }
    }
}

/// One local day, from midnight to midnight, in the day's zone.
fn day(rng: &mut StdRng, places: &Places, date: NaiveDate, tz: Tz) -> Day {
    let at = |h: u32, m: u32| -> DateTime<Utc> {
        tz.from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
            .single()
            .unwrap()
            .with_timezone(&Utc)
    };
    let midnight = at(0, 0);
    let next_midnight = tz
        .from_local_datetime(&(date + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc);
    let mut points = Vec::new();
    let mut segments = Vec::new();
    let mut now = midnight;
    let mut here = places.home;
    let weekend = matches!(date.weekday(), Weekday::Sat | Weekday::Sun);

    // A plan: (destination, mode there, minutes of stay). The day ends at home.
    let mut plan: Vec<(Geo, Mode, i64)> = Vec::new();
    if weekend {
        match rng.random_range(0..4) {
            0 => plan.push((places.park, Mode::Walk, 25)),
            1 => plan.push((places.lake, Mode::Car, 150)),
            2 => {
                plan.push((places.park, Mode::Bike, 10));
                plan.push((places.lake, Mode::Bike, 60));
            }
            _ => plan.push((places.park, Mode::Run, 5)),
        }
        plan.push((places.shop, Mode::Walk, 20));
    } else {
        let commute = if rng.random_range(0.0..1.0) < 0.6 {
            Mode::Bike
        } else {
            Mode::Car
        };
        plan.push((places.work, commute, 505 + rng.random_range(-40..40)));
        if rng.random_range(0.0..1.0) < 0.4 {
            plan.push((places.gym, Mode::Walk, 70));
        }
        if rng.random_range(0.0..1.0) < 0.5 {
            plan.push((places.shop, Mode::Walk, 15));
        }
    }
    plan.push((places.home, if weekend { Mode::Walk } else { plan[0].1 }, 0));

    let first_departure =
        if weekend { at(9, 30) } else { at(7, 45) } + Duration::minutes(rng.random_range(-20..20));
    let (stay_points, stay) = stop(rng, places.home, now, first_departure);
    points.extend(stay_points);
    segments.push(stay);
    now = first_departure;

    for (destination, mode, minutes) in plan {
        let (trip_points, trip_segment) = trip(rng, here, destination, mode, now);
        now = trip_segment.end;
        points.extend(trip_points);
        segments.push(trip_segment);
        here = destination;
        if minutes > 0 {
            let leave = now + Duration::minutes(minutes);
            let (stay_points, stay) = stop(rng, here, now, leave);
            points.extend(stay_points);
            segments.push(stay);
            now = leave;
        }
    }
    let (stay_points, stay) = stop(rng, places.home, now, next_midnight);
    points.extend(stay_points);
    segments.push(stay);
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
    let mut rng = StdRng::seed_from_u64(args.seed);
    let places = Places::around(Geo {
        lat: args.lat,
        lon: args.lon,
    });

    let user_id: Uuid = match sqlx::query_scalar("select id from users where email = $1")
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
        let generated = day(&mut rng, &places, date, tz);
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
    fn a_walk_of_two_kilometres_takes_about_twenty_four_minutes_at_ten_metre_spacing() {
        let mut rng = StdRng::seed_from_u64(7);
        let home = Geo {
            lat: 52.2297,
            lon: 21.0122,
        };
        let (points, segment) = trip(
            &mut rng,
            home,
            home.offset(1_400.0, 1_400.0),
            Mode::Walk,
            Utc::now(),
        );
        let minutes = (segment.end - segment.start).num_minutes();
        assert!((18..=32).contains(&minutes), "{minutes} minutes");
        assert!(
            points.len() > 150 && points.len() < 260,
            "{} points",
            points.len()
        );
        assert!(
            segment.distance_m > 1_900 && segment.distance_m < 2_400,
            "{} m",
            segment.distance_m
        );
        assert!(segment.steps.unwrap() > 2_000);
    }

    #[test]
    fn a_weekday_starts_and_ends_at_home_with_a_stay_at_work() {
        let mut rng = StdRng::seed_from_u64(1);
        let places = Places::around(Geo {
            lat: 52.2297,
            lon: 21.0122,
        });
        let generated = day(
            &mut rng,
            &places,
            NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            chrono_tz::Europe::Warsaw,
        );
        let kinds: Vec<&str> = generated.segments.iter().map(|s| s.kind).collect();
        assert_eq!(kinds.first(), Some(&"stop"));
        assert_eq!(kinds.last(), Some(&"stop"));
        assert!(
            generated
                .segments
                .iter()
                .any(|s| s.kind == "stop" && (s.end - s.start).num_minutes() > 400)
        );
        let last = generated.segments.last().unwrap();
        assert!(last.end > last.start);
    }

    #[test]
    fn the_same_seed_gives_the_same_month() {
        let places = Places::around(Geo {
            lat: 52.2297,
            lon: 21.0122,
        });
        let date = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap();
        let a = day(
            &mut StdRng::seed_from_u64(5),
            &places,
            date,
            chrono_tz::Europe::Warsaw,
        );
        let b = day(
            &mut StdRng::seed_from_u64(5),
            &places,
            date,
            chrono_tz::Europe::Warsaw,
        );
        assert_eq!(a.points.len(), b.points.len());
        assert_eq!(a.points[10].geo.scaled(), b.points[10].geo.scaled());
    }
}
