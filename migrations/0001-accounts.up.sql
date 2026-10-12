-- 0001 accounts: up. The five account tables; constraint and index names are set by hand so
-- the API can map a violation to an error by name.
create extension if not exists citext;

create table users (
    id uuid primary key default uuidv7(),
    email citext not null,
    password_hash text not null,
    created_at timestamptz not null default now(),
    deleted_at timestamptz null,
    constraint users_email_key unique (email),
    constraint users_email_check check (email ~ '^[^@[:space:]]+@[^@[:space:]]+$')
);

create table invite_codes (
    code text primary key,
    created_by uuid null constraint invite_codes_created_by_fkey references users (id) on delete set null,
    created_at timestamptz not null default now(),
    used_by uuid null constraint invite_codes_used_by_fkey references users (id) on delete set null,
    used_at timestamptz null,
    constraint invite_codes_used_check check ((used_by is null) = (used_at is null))
);

create table devices (
    id uuid primary key,
    user_id uuid not null constraint devices_user_id_fkey references users (id) on delete cascade,
    name text not null,
    platform text not null,
    app_version text not null,
    created_at timestamptz not null default now(),
    last_seen_at timestamptz not null default now(),
    constraint devices_platform_check check (platform in ('ios'))
);
create index devices_user_id_idx on devices (user_id);

create table refresh_tokens (
    id uuid primary key default uuidv7(),
    device_id uuid not null constraint refresh_tokens_device_id_fkey references devices (id) on delete cascade,
    token_hash text not null,
    issued_at timestamptz not null default now(),
    expires_at timestamptz not null,
    revoked_at timestamptz null,
    constraint refresh_tokens_token_hash_key unique (token_hash)
);
create index refresh_tokens_device_id_idx on refresh_tokens (device_id);

create table password_resets (
    id uuid primary key default uuidv7(),
    user_id uuid not null constraint password_resets_user_id_fkey references users (id) on delete cascade,
    token_hash text not null,
    expires_at timestamptz not null,
    used_at timestamptz null,
    constraint password_resets_token_hash_key unique (token_hash)
);
create index password_resets_user_id_idx on password_resets (user_id);
