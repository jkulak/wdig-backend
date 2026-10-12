#!/usr/bin/env bash
# Drop the local wdig database and build it again from the migrations. Refuses anything but
# this checkout's compose project on the local engine: it never touches another database.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
case "${DOCKER_HOST:-}" in ""|unix://*) ;; *) echo "refused: DOCKER_HOST points at a remote engine" >&2; exit 1 ;; esac
container=$(docker compose ps -q db 2>/dev/null || true)
[ -n "$container" ] || { echo "refused: the local db is not running; make up first" >&2; exit 1; }
project=$(docker inspect -f '{{ index .Config.Labels "com.docker.compose.project" }}' "$container")
[ "$project" = wdig-backend ] || { echo "refused: the db belongs to the compose project $project" >&2; exit 1; }
docker compose stop app >/dev/null 2>&1 || true
docker exec -i "$container" psql -v ON_ERROR_STOP=1 -q -U wdig -d postgres \
  -c "drop database if exists wdig with (force)" -c "create database wdig"
./scripts/migrate.sh up
docker compose start app >/dev/null 2>&1 || true
echo "ok the local wdig database was rebuilt from migrations/"
