-- 0002 tracking: up. What the phone records and computes, mirrored for good: raw points
-- partitioned by month, the segments of both methods, the owner's corrections, places and
-- markers, and the batch records that make uploads and imports idempotent.
create table import_batches (
    id uuid primary key default uuidv7(),
    user_id uuid not null constraint import_batches_user_id_fkey references users (id) on delete cascade,
    source text not null,
    file_name text not null,
    imported_at timestamptz not null default now(),
    stats jsonb not null default '{}'
);

create table upload_batches (
    id uuid primary key,
    device_id uuid not null constraint upload_batches_device_id_fkey references devices (id) on delete cascade,
    received_at timestamptz not null default now(),
    point_count int4 not null,
    accepted int4 not null,
    duplicates int4 not null
);

create table places (
    id uuid primary key,
    user_id uuid not null constraint places_user_id_fkey references users (id) on delete cascade,
    name text not null,
    lat int4 not null,
    lon int4 not null,
    radius_m int2 not null default 100,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    synced_at timestamptz not null default now(),
    deleted_at timestamptz null,
    constraint places_lat_check check (lat between -900000000 and 900000000),
    constraint places_lon_check check (lon between -1800000000 and 1800000000),
    constraint places_radius_m_check check (radius_m between 10 and 5000)
);
create index places_user_id_updated_at_idx on places (user_id, updated_at);
create index places_user_id_synced_at_idx on places (user_id, synced_at);

create table points (
    user_id uuid not null constraint points_user_id_fkey references users (id) on delete cascade,
    device_id uuid not null constraint points_device_id_fkey references devices (id) on delete cascade,
    recorded_at timestamptz not null,
    tz text not null,
    lat int4 not null,
    lon int4 not null,
    h_acc_m int2 not null,
    alt_m int2 null,
    v_acc_m int2 null,
    speed_cms int2 not null default -1,
    course_deg int2 not null default -1,
    activity int2 not null,
    activity_conf int2 not null,
    source text not null,
    battery_pct int2 null,
    battery_state int2 null,
    coarse bool not null default false,
    import_batch_id uuid null constraint points_import_batch_id_fkey references import_batches (id),
    primary key (device_id, recorded_at),
    constraint points_lat_check check (lat between -900000000 and 900000000),
    constraint points_lon_check check (lon between -1800000000 and 1800000000),
    constraint points_h_acc_m_check check (h_acc_m >= 0),
    constraint points_speed_cms_check check (speed_cms >= -1),
    constraint points_course_deg_check check (course_deg = -1 or course_deg between 0 and 359),
    constraint points_activity_check check (activity between 0 and 5),
    constraint points_activity_conf_check check (activity_conf between 0 and 2),
    constraint points_source_check check (source in ('phone', 'visit', 'significant_change') or source like 'import:%'),
    constraint points_battery_pct_check check (battery_pct is null or battery_pct between 0 and 100),
    constraint points_battery_state_check check (battery_state is null or battery_state between 0 and 3)
) partition by range (recorded_at);
create index points_recorded_at_brin on points using brin (recorded_at) with (pages_per_range = 32);
create index points_user_id_recorded_at_idx on points (user_id, recorded_at);
create table points_2026_10 partition of points for values from ('2026-10-01 00:00:00+00') to ('2026-11-01 00:00:00+00');
create table points_2026_11 partition of points for values from ('2026-11-01 00:00:00+00') to ('2026-12-01 00:00:00+00');
create table points_2026_12 partition of points for values from ('2026-12-01 00:00:00+00') to ('2027-01-01 00:00:00+00');
create table points_2027_01 partition of points for values from ('2027-01-01 00:00:00+00') to ('2027-02-01 00:00:00+00');
create table points_default partition of points default;

create table segments (
    id uuid primary key default uuidv7(),
    user_id uuid not null constraint segments_user_id_fkey references users (id) on delete cascade,
    device_id uuid null constraint segments_device_id_fkey references devices (id) on delete set null,
    kind text not null,
    start_at timestamptz not null,
    end_at timestamptz not null,
    tz text not null,
    mode_detected text not null,
    mode_conf text not null,
    mode_type text not null,
    distance_m int4 not null default 0,
    point_count int4 not null default 0,
    center_lat int4 null,
    center_lon int4 null,
    place_id uuid null constraint segments_place_id_fkey references places (id) on delete set null,
    source text not null,
    computed_by text not null,
    algorithm_version int2 not null,
    steps int4 null,
    coarse bool not null default false,
    updated_at timestamptz not null default now(),
    synced_at timestamptz not null default now(),
    deleted_at timestamptz null,
    constraint segments_kind_check check (kind in ('stop', 'trip')),
    constraint segments_range_check check (end_at >= start_at),
    constraint segments_mode_detected_check check (mode_detected in ('stationary', 'walking', 'running', 'cycling', 'automotive', 'unknown')),
    constraint segments_mode_conf_check check (mode_conf in ('low', 'medium', 'high')),
    constraint segments_mode_type_check check (mode_type in ('walk', 'run', 'scooter', 'bike', 'motorcycle', 'car', 'train', 'plane', 'other')),
    constraint segments_distance_m_check check (distance_m >= 0),
    constraint segments_point_count_check check (point_count >= 0),
    constraint segments_center_check check (kind <> 'stop' or (center_lat is not null and center_lon is not null)),
    constraint segments_center_lat_check check (center_lat is null or center_lat between -900000000 and 900000000),
    constraint segments_center_lon_check check (center_lon is null or center_lon between -1800000000 and 1800000000),
    constraint segments_computed_by_check check (computed_by in ('phone', 'server')),
    constraint segments_steps_check check (steps is null or steps >= 0)
);
create index segments_user_id_computed_by_start_at_idx on segments (user_id, computed_by, start_at);
create index segments_user_id_updated_at_idx on segments (user_id, updated_at);
create index segments_user_id_synced_at_idx on segments (user_id, synced_at);

create table corrections (
    id uuid primary key,
    user_id uuid not null constraint corrections_user_id_fkey references users (id) on delete cascade,
    kind text not null,
    start_at timestamptz not null,
    end_at timestamptz not null,
    payload jsonb not null default '{}',
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    synced_at timestamptz not null default now(),
    deleted_at timestamptz null,
    constraint corrections_kind_check check (kind in ('retype', 'split', 'merge', 'promote_stop', 'delete'))
);
create index corrections_user_id_updated_at_idx on corrections (user_id, updated_at);
create index corrections_user_id_synced_at_idx on corrections (user_id, synced_at);

create table markers (
    id uuid primary key,
    user_id uuid not null constraint markers_user_id_fkey references users (id) on delete cascade,
    recorded_at timestamptz not null,
    tz text not null,
    lat int4 not null,
    lon int4 not null,
    note text not null default '',
    photo_ref text null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    synced_at timestamptz not null default now(),
    deleted_at timestamptz null,
    constraint markers_lat_check check (lat between -900000000 and 900000000),
    constraint markers_lon_check check (lon between -1800000000 and 1800000000)
);
create index markers_user_id_updated_at_idx on markers (user_id, updated_at);
create index markers_user_id_synced_at_idx on markers (user_id, synced_at);
