#!/usr/bin/env bash
#
# Database migrations: plain SQL, two files each, applied in order, recorded in the database.
#
#   migrations/NNNN-slug.up.sql      what the migration does
#   migrations/NNNN-slug.down.sql    how to undo it; every migration has one
#
# Applied versions are recorded in schema_migrations in the database itself, so a migration
# runs once and status is a query. Each file runs as one transaction (psql
# --single-transaction), and Postgres rolls DDL back with the rest, so a file that fails
# leaves nothing behind. Keep a migration small all the same.
#
#   migrate.sh status             every migration: applied when, or pending
#   migrate.sh up [<version>]     apply what is pending, up to and including <version>
#   migrate.sh down [<n>|all]     revert the last <n> applied migrations, one by default
#   migrate.sh new <slug>         two empty files with the next version number
#
# Where it runs, MIGRATE_TARGET:
#   local       this checkout's compose project on the local Docker engine (the default)
#   ovh         production's database on the OVH host, over ssh as the admin account, with
#               OVH_HOST from gcloud-ovh-migrate's .env; `down` asks for a typed confirmation
# MIGRATE_DB names another database on the same server (the tests use wdig_test).
#
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

DIR=${MIGRATIONS_DIR:-migrations}
TARGET=${MIGRATE_TARGET:-local}
DB=${MIGRATE_DB:-wdig}
TABLE=schema_migrations
cmd=${1:-}
arg=${2:-}

refuse() { echo "refused: $*" >&2; exit 1; }
usage() { echo "usage: $0 {status | up [<version>] | down [<n>|all] | new <slug>}" >&2; exit 2; }
case "$cmd" in status|up|down|new) ;; *) usage ;; esac

[ -d "$DIR" ] || refuse "no migrations directory $DIR"
migrations() {
    local f base version name previous=
    for f in "$DIR"/[0-9][0-9][0-9][0-9]-*.up.sql; do
        [ -e "$f" ] || continue
        base=$(basename "$f" .up.sql)
        version=${base%%-*}
        name=${base#*-}
        [[ "$name" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || refuse "$f: a slug is lowercase letters, digits and dashes"
        [ -f "$DIR/$base.down.sql" ] || refuse "$f has no $base.down.sql; every migration has a down"
        [ "$version" != "$previous" ] || refuse "two migrations carry the version $version in $DIR"
        previous=$version
        printf '%s\t%s\n' "$version" "$name"
    done
}
ALL=$(migrations)
name_of() { printf '%s\n' "$ALL" | awk -F'\t' -v v="$1" '$1 == v { print $2 }'; }

if [ "$cmd" = new ]; then
    [[ "$arg" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] || refuse "a slug is lowercase letters, digits and dashes, like add-steps"
    last=$(printf '%s\n' "$ALL" | awk -F'\t' 'END { print $1 + 0 }')
    next=$(printf '%04d' $((last + 1)))
    printf -- '-- %s %s: up. One change, small enough to undo with the down file next to it.\n' "$next" "$arg" >"$DIR/$next-$arg.up.sql"
    printf -- '-- %s %s: down. Undo exactly what the up file does.\n' "$next" "$arg" >"$DIR/$next-$arg.down.sql"
    echo "ok $DIR/$next-$arg.up.sql and $next-$arg.down.sql; fill them in, then make migrate"
    exit 0
fi

# --- The database -------------------------------------------------------------------------
case "$TARGET" in
    local)
        where="the local stack's db, database $DB"
        container=$(docker compose ps -q db 2>/dev/null || true)
        [ -n "$container" ] || refuse "the local db is not running; make up first"
        psql_cmd() { docker exec -i "$container" psql -v ON_ERROR_STOP=1 -q -U wdig -d "$DB" "$@"; }
        ;;
    ovh)
        host=${OVH_HOST:-$(sed -n 's/^OVH_HOST=//p' ../gcloud-ovh-migrate/.env 2>/dev/null | head -1)}
        [ -n "$host" ] || refuse "OVH_HOST is not set and ../gcloud-ovh-migrate/.env has none"
        where="production's db on $host, database $DB"
        psql_cmd() { ssh "ubuntu@$host" docker exec -i wdig-db-1 psql -v ON_ERROR_STOP=1 -q -U wdig -d "$DB" "$@"; }
        ;;
    *) refuse "MIGRATE_TARGET is $TARGET; local or ovh" ;;
esac

query() { printf '%s\n' "$1" | psql_cmd -At; }
run_file() { psql_cmd --single-transaction <"$1"; }

[ "$(query 'select 1' 2>/dev/null)" = 1 ] || refuse "cannot reach $where"
query "create table if not exists $TABLE (version char(4) primary key, name text not null, applied_at timestamptz not null default now())" >/dev/null
applied() { query "select version || E'\t' || name || E'\t' || to_char(applied_at, 'YYYY-MM-DD HH24:MI:SS') from $TABLE order by version"; }
is_recorded() { printf '%s\n' "$2" | awk -F'\t' -v v="$1" '$1 == v' | grep -q .; }

status() {
    local recs version name rec state done=0 pending=0
    recs=$(applied)
    echo "migrations in $DIR, $where"
    printf '  %-8s %-28s %s\n' version name applied
    while IFS=$'\t' read -r version name; do
        [ -n "$version" ] || continue
        rec=$(printf '%s\n' "$recs" | awk -F'\t' -v v="$version" '$1 == v { print $3 }')
        if [ -n "$rec" ]; then state=$rec; done=$((done + 1)); else state=pending; pending=$((pending + 1)); fi
        printf '  %-8s %-28s %s\n' "$version" "$name" "$state"
    done <<<"$ALL"
    echo "$done applied, $pending pending"
}

up() {
    local to=$1 recs version name n=0
    recs=$(applied)
    while IFS=$'\t' read -r version name; do
        [ -n "$version" ] || continue
        [ -z "$to" ] || [ "$version" \< "$to" ] || [ "$version" = "$to" ] || break
        is_recorded "$version" "$recs" && continue
        echo "up   $version $name"
        run_file "$DIR/$version-$name.up.sql"
        query "insert into $TABLE (version, name) values ('$version', '$name')" >/dev/null
        n=$((n + 1))
    done <<<"$ALL"
    echo "ok $n applied on $where"
}

down() {
    local n=$1 recs version name count=0
    recs=$(applied)
    [ -n "$recs" ] || { echo "nothing applied on $where"; return; }
    [ "$n" = all ] && n=$(printf '%s\n' "$recs" | grep -c .)
    if [ "$TARGET" = ovh ]; then
        echo "about to revert $n migration(s) on $where; type the database name to continue:"
        read -r confirm; [ "$confirm" = "$DB" ] || refuse "not confirmed"
    fi
    while [ "$count" -lt "$n" ]; do
        version=$(printf '%s\n' "$recs" | awk -F'\t' 'END { print $1 }')
        [ -n "$version" ] || break
        name=$(name_of "$version")
        [ -n "$name" ] || refuse "$version is recorded but has no file in $DIR"
        echo "down $version $name"
        run_file "$DIR/$version-$name.down.sql"
        query "delete from $TABLE where version = '$version'" >/dev/null
        recs=$(printf '%s\n' "$recs" | awk -F'\t' -v v="$version" '$1 != v')
        count=$((count + 1))
    done
    echo "ok $count reverted on $where"
}

case "$cmd" in
    status) status ;;
    up) up "$arg" ;;
    down) down "${arg:-1}" ;;
esac
