//! `points` is partitioned by month. A missing partition would fail every insert of that
//! month, so the service creates the current month and the next few at start and once a
//! day, and the operator can run the same from the command line.

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDate, Utc};
use sqlx::PgPool;

pub fn parse_day(text: Option<&str>) -> Result<NaiveDate> {
    match text {
        Some(text) => {
            NaiveDate::parse_from_str(text, "%Y-%m-%d").context("--today takes YYYY-MM-DD")
        }
        None => Ok(Utc::now().date_naive()),
    }
}

/// The first day of the month `offset` months after `day`'s month.
pub fn month_start(day: NaiveDate, offset: u32) -> NaiveDate {
    let months = day.year() * 12 + day.month0() as i32 + offset as i32;
    NaiveDate::from_ymd_opt(months.div_euclid(12), (months.rem_euclid(12) + 1) as u32, 1)
        .expect("a valid month")
}

pub fn partition_name(start: NaiveDate) -> String {
    format!("points_{:04}_{:02}", start.year(), start.month())
}

/// Creates the partitions for `day`'s month and `ahead` months after it. Prints one line per
/// partition when `verbose`, and warns when the default partition holds rows.
pub async fn ensure(pool: &PgPool, day: NaiveDate, ahead: u32, verbose: bool) -> Result<()> {
    let have_points: bool = sqlx::query_scalar("select to_regclass('points') is not null")
        .fetch_one(pool)
        .await?;
    if !have_points {
        bail!("the points table does not exist yet; apply the migrations first");
    }
    for offset in 0..=ahead {
        let start = month_start(day, offset);
        let end = month_start(day, offset + 1);
        let name = partition_name(start);
        let existed: bool = sqlx::query_scalar("select to_regclass($1) is not null")
            .bind(&name)
            .fetch_one(pool)
            .await?;
        if !existed {
            let sql = format!(
                "create table if not exists {name} partition of points for values from ('{start} 00:00:00+00') to ('{end} 00:00:00+00')"
            );
            sqlx::query(&sql)
                .execute(pool)
                .await
                .with_context(|| format!("creating {name}"))?;
        }
        if verbose {
            println!("{} {name}", if existed { "exists" } else { "created" });
        } else if !existed {
            tracing::info!(partition = %name, "created");
        }
    }
    let stray: Vec<(String,)> = sqlx::query_as(
        "select distinct to_char(recorded_at at time zone 'UTC', 'YYYY-MM') from points_default order by 1",
    )
    .fetch_all(pool)
    .await?;
    if !stray.is_empty() {
        let months: Vec<String> = stray.into_iter().map(|row| row.0).collect();
        let message = format!(
            "points_default holds rows for {}; create their partitions and move them",
            months.join(", ")
        );
        if verbose {
            eprintln!("warning: {message}");
        } else {
            tracing::warn!("{message}");
        }
    }
    Ok(())
}

/// Runs `ensure` now and then every 24 hours, for as long as the service lives; after a
/// failure (the database not up yet, the migrations not applied yet) it tries again in a minute.
pub fn spawn_keeper(pool: PgPool) {
    tokio::spawn(async move {
        loop {
            let wait = match ensure(&pool, Utc::now().date_naive(), 3, false).await {
                Ok(()) => 24 * 3600,
                Err(error) => {
                    tracing::warn!(error = %error, "partitions: trying again in a minute");
                    60
                }
            };
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_month_after_december_is_january_of_the_next_year() {
        let day = NaiveDate::from_ymd_opt(2026, 12, 15).unwrap();
        assert_eq!(
            month_start(day, 0),
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()
        );
        assert_eq!(
            month_start(day, 1),
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap()
        );
        assert_eq!(
            month_start(day, 14),
            NaiveDate::from_ymd_opt(2028, 2, 1).unwrap()
        );
    }

    #[test]
    fn a_partition_is_named_after_its_month() {
        assert_eq!(
            partition_name(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
            "points_2026_10"
        );
    }
}
