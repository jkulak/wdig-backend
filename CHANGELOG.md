# Changelog

All notable changes to wdig-backend, newest first. Releases are tagged `vYYYY.MM.N` by the
release workflow on every merge to `main`; the note of a release is what this file gained.
Lines under `### Internal` are for the repository, not for a release note.

## Unreleased

### Added

- The ingest API: `POST /api/v1/points/batches` stores a batch of the phone's points once,
  whatever the number of retries (a replayed batch id answers the first counts and stores
  nothing, a point already there is a duplicate), and `PUT /api/v1/segments`, `/corrections`,
  `/places` and `/markers` upsert rows by id with the phone's `updated_at` deciding and the
  server's `synced_at` recording when; every field checked at the door with the row and the
  field named in the 422; 120 requests a minute per account.
- The accounts API: `POST /api/v1/auth/register` with an invite code (`wdig invite --count N`
  prints them), `login` that registers the phone and issues an access token (15 min, HS256) with
  a refresh token (90 days, rotated on every `refresh`, revoked by `logout`), argon2id passwords,
  the same answer and time for a wrong e-mail and a wrong password, and a login limit per
  address and per account.
- The service: `GET /api/v1/health` with the database's state and the build's version, the
  schema (accounts, and the tracking tables with `points` partitioned by month), a partition
  keeper that runs at start and once a day, and `wdig seed`, a made-up month for an account
  over Warsaw's real streets (Apple Maps routes in `fixtures/demo_routes.json`, shared with the
  app's demo month), sampled the way the recorder keeps fixes.

### Internal

- Rust stable on Debian 13 (`rust:1-slim-trixie`, `debian:trixie-slim`), Postgres 18, logs as
  one JSON object per line with `time`, `level` and `msg`; plain-SQL migrations with a `down`
  each, applied by `scripts/migrate.sh`; one CI job on pull requests.
