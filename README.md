# wdig-backend

The backend of WDIG (Where did I go?): user accounts, a mirror of everything the phone records,
and a second segmentation of the raw points, so the two methods can be compared. Rust (axum,
sqlx) on Postgres 18, in Docker, on the shared OVH host at `wdig.itsnotezra.com` (the API under
`/api/v1`, the web at `/`).

## Running it here

There is no cargo and no psql on the host: every command runs in a container.

```bash
cp compose.override.example.yaml compose.override.yaml   # once: the builder stage with the source mounted
make up            # the database and the service on http://localhost:8090
make migrate       # apply migrations/ to the local database
make seed EMAIL=kuba@example.com DAYS=30   # a made-up month over Warsaw's streets to look at
make test          # cargo test against a throwaway wdig_test database
make lint          # clippy, warnings as errors
make cs            # rustfmt --check
make help          # everything else
```

`curl http://localhost:8090/api/v1/health` answers `{"status":"ok","db":"ok","version":"..."}`.

## Migrations

Plain SQL in `migrations/`, `NNNN-slug.up.sql` with its `.down.sql`, applied in order and
recorded in `schema_migrations`; `make migrate-new NAME=add-steps` starts the next one. Each
file runs as one transaction. Production gets them with `make ovh-migrate` before the release
that needs them (`deploy/ovh/README.md`).

## Deploying

`deploy/ovh/` is the shared host's service template (jkulak/gcloud-ovh-migrate, CONTRACT.md):
a merge to `main` releases itself and the deploy workflow rolls the image out in two colours.
The task files in `issues/` (uncommitted) say what is built in which order.
