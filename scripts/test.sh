#!/usr/bin/env bash
# cargo test in the builder image, on the compose network, against a wdig_test database the
# script creates on the stack's db and drops afterwards. The stack's db has to be running.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
docker compose up -d --wait db
docker compose exec -T db psql -v ON_ERROR_STOP=1 -U wdig -d postgres -c "drop database if exists wdig_test" -c "create database wdig_test" >/dev/null
cleanup() { docker compose exec -T db psql -U wdig -d postgres -c "drop database if exists wdig_test" >/dev/null 2>&1 || true; }
trap cleanup EXIT
MIGRATE_DB=wdig_test ./scripts/migrate.sh up >/dev/null
docker run --rm -v "$PWD:/app" -v wdig-backend-cargo-registry:/usr/local/cargo/registry \
  -v wdig-backend-cargo-target:/app/target -w /app --network wdig-backend_default \
  -e DATABASE_URL=postgres://wdig:wdig@db:5432/wdig_test \
  -e TOKEN_SIGNING_KEY=test-signing-key-of-at-least-thirty-two-bytes \
  wdig-backend-builder cargo test "$@"
