# Changelog

All notable changes to wdig-backend, newest first. Releases are tagged `vYYYY.MM.N` by the
release workflow on every merge to `main`; the note of a release is what this file gained.
Lines under `### Internal` are for the repository, not for a release note.

## Unreleased

### Added

- The service: `GET /api/v1/health` with the database's state and the build's version, the
  schema (accounts, and the tracking tables with `points` partitioned by month), a partition
  keeper that runs at start and once a day, and `wdig seed`, a made-up month for an account.

### Internal

- Rust stable on Debian 13 (`rust:1-slim-trixie`, `debian:trixie-slim`), Postgres 18, logs as
  one JSON object per line with `time`, `level` and `msg`; plain-SQL migrations with a `down`
  each, applied by `scripts/migrate.sh`; one CI job on pull requests.
