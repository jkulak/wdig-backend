#!/usr/bin/env bash
# The stack on this Mac as the shared OVH host would run it, behind the edge's real
# configuration: first what can be read off the files (gcloud-ovh-migrate's CONTRACT.md), then
# the stack itself, built from this checkout, started and asked for each of its names through
# a local edge, then, for a request path in two colours, the edge moved from blue to green
# under a stream of requests, as a deploy moves it, and last what its own images wrote while it
# did. Nothing on this machine named `edge` is touched.
#
# From gcloud-ovh-migrate's service template, word for word the same in every service. The
# edge's configuration is read from that repo's checkout: OVH_PLATFORM, which deploy/ovh/ovh.mk
# sets, or ~/Developer/gcloud-ovh-migrate.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 1
# shellcheck source=/dev/null
. deploy/ovh/service.env
: "${DOCKER:=docker}" "${CADDY_IMAGE:=caddy:2.11.4-alpine}"
PLATFORM=${OVH_PLATFORM:-$HOME/Developer/gcloud-ovh-migrate}
[ -f "$PLATFORM/edge/Caddyfile" ] \
  || { echo "x no edge configuration at $PLATFORM/edge/Caddyfile; set OVH_PLATFORM to a gcloud-ovh-migrate checkout" >&2; exit 1; }

pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1));
          else echo "x    $1 (expected [$2], got [$3])"; fail=$((fail+1)); fi; }

ID=$$ T=$(mktemp -d)
PROJECT=$SERVICE-stacktest-$ID NET=edge-stacktest-$ID EDGE=edge-stacktest-$ID
built=()
cleanup() {
  # Through compose(), with the stand-ins: without them the file does not render, and down
  # would fail quietly and leave the stack running.
  compose down -v --remove-orphans >/dev/null 2>&1
  "$DOCKER" rm -f "$EDGE" >/dev/null 2>&1
  "$DOCKER" network rm "$NET" >/dev/null 2>&1
  for image in ${built[@]+"${built[@]}"}; do "$DOCKER" image rm "$image" >/dev/null 2>&1; done
  rm -rf "$T"
  echo "cleaned up: project $PROJECT, edge $EDGE, network $NET${built[*]+, images ${built[*]}}"
}
trap cleanup EXIT

# Every variable the compose file requires, given a stand-in, so it renders as on the host.
required=$(grep -oE '\$\{[A-Za-z_][A-Za-z0-9_]*:\?' deploy/ovh/compose.yaml | sed -E 's/^\$\{//; s/:\?$//' | sort -u)
stand_ins=(IMAGE_TAG=stack-test)
for v in $required; do [ "$v" = IMAGE_TAG ] || stand_ins+=("$v=stack-test-$v"); done
printf 'networks:\n  edge:\n    name: %s\n    external: true\n' "$NET" > "$T/override.yaml"
# What has to differ for the stack to run on a Mac with no outside accounts — a sync client
# with nothing to sign in to, fresh volumes to hand over — is the service's to say, in an
# optional deploy/ovh/stack-test.yaml. It shapes the running stack only: the checks of the
# files below read deploy/ovh/compose.yaml as the host runs it.
files=(-f deploy/ovh/compose.yaml)
[ ! -f deploy/ovh/stack-test.yaml ] || files+=(-f deploy/ovh/stack-test.yaml)
files+=(-f "$T/override.yaml")
compose() { env "${stand_ins[@]}" "$DOCKER" compose -p "$PROJECT" "${files[@]}" "$@"; }

# ---- what the files say --------------------------------------------------------
# Every profile on: a job that runs and exits carries one (CONTRACT.md §6), and its image and
# settings are the host's as much as the rest. `up` further down starts none of them, as the
# host's never does.
env "${stand_ins[@]}" "$DOCKER" compose -f deploy/ovh/compose.yaml --profile '*' config --format json > "$T/config.json" 2>"$T/config.err"
check "the compose file renders with every required value given" 0 "$?"
[ -s "$T/config.json" ] || { cat "$T/config.err" >&2; echo; echo "$pass passed, $fail failed"; exit 1; }

# A secret compose hands a container from a variable (top-level secrets: with environment:) is
# required at `up` as much as a ${X:?} one: compose refuses to start without it.
for v in $(python3 -c '
import json, sys
print(" ".join(sorted({s["environment"] for s in (json.load(open(sys.argv[1])).get("secrets") or {}).values() if s.get("environment")})))' "$T/config.json"); do
  stand_ins+=("$v=stack-test-$v")
done
# An app that checks what its settings look like — a key that has to be base64, a URL — starts
# on values that pass, for the test alone, from deploy/ovh/stack-test.env: KEY=value lines, a
# line break written \n. They come last, so they win over the stand-ins above.
if [ -f deploy/ovh/stack-test.env ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in '#'*|'') continue ;; *=*) ;; *) continue ;; esac
    stand_ins+=("${line%%=*}=$(printf '%b' "${line#*=}")")
  done < deploy/ovh/stack-test.env
fi
# What a port published on the host's loopback would bind here is nothing the test needs: the
# edge reaches the stack over its network. So the override takes those ports away.
python3 - "$T/config.json" "$NET" > "$T/override.yaml" <<'PY'
import json, sys
cfg, net = json.load(open(sys.argv[1])), sys.argv[2]
print("networks:\n  edge:\n    name: %s\n    external: true" % net)
published = [n for n, s in cfg.get("services", {}).items() if s.get("ports")]
if published:
    print("services:")
    for n in published:
        print("  %s:\n    ports: !reset []" % n)
PY

# The images this repo builds, under the references compose will ask for.
images=$(python3 -c '
import json, sys
for s in json.load(open(sys.argv[1])).get("services", {}).values():
    if s.get("image"): print(s["image"])' "$T/config.json" | sort -u)
# A pipe, not a here-string: bash 3.2 reads <<< inside $( ) as the start of a here-document.
own=$(for spec in ${IMAGES:-}; do printf '%s\n' "$images" | grep -E "/${spec%%=*}:stack-test$" | head -1; done)

# Each row is label, expected and actual, apart by \x1f: a tab is white space to read, so two
# in a row, around an expected "", would run together and shift the actual into its place.
# The programs that read the files go to files of their own first: bash 3.2, the Mac's, brace-
# expands a here-document inside <( ) or $( ), and a Python dict or set with a comma in it
# comes apart, which once kept every check below from running at all.
cat > "$T/compose-checks.py" <<'PY'
import json, sys
cfg, service, own = json.load(open(sys.argv[1])), sys.argv[2], set(sys.argv[3].split())
services, volumes = cfg.get("services", {}), set(cfg.get("volumes") or {})
def row(label, expected, actual): print(f"{label}\x1f{expected}\x1f{actual}")
row("nothing publishes a port beyond 127.0.0.1", "",
    " ".join(n for n, s in services.items() if any(p.get("host_ip") != "127.0.0.1" for p in s.get("ports") or [])))
row("no container reads an env file", "", " ".join(n for n, s in services.items() if s.get("env_file")))
row("every long-running container has a healthcheck, in compose or in an image built here", "",
    " ".join(n for n, s in services.items()
             if not s.get("profiles") and not (s.get("healthcheck") or {}).get("test") and s.get("image") not in own))
row("the edge network is the shared one", "edge true",
    f'{(cfg.get("networks", {}).get("edge") or {}).get("name")} {json.dumps((cfg.get("networks", {}).get("edge") or {}).get("external"))}')
on_edge = {n: (s.get("networks") or {}).get("edge") for n, s in services.items() if "edge" in (s.get("networks") or {})}
row("something joins the edge", "yes", "yes" if on_edge else "no")
row(f"everything on the edge answers to a name of its own, {service}-…", "",
    " ".join(n for n, e in on_edge.items() if not any(a.startswith(f"{service}-") for a in ((e or {}).get("aliases") or []))))
for n, s in services.items():
    keys = ((s.get("labels") or {}).get("backup.volumes") or "").split()
    mounted = {v.get("source") for v in s.get("volumes") or [] if v.get("type") == "volume"}
    row(f"{n}: every volume it names for the backup is one it mounts", "", " ".join(k for k in keys if k not in mounted or k not in volumes))
# A request path in two colours (CONTRACT.md §2): <name>-blue and <name>-green, alike, each on
# its own tag, one per colour naming the edge's upstream, an alias it has on the edge.
coloured = {n: s for n, s in services.items() if (s.get("labels") or {}).get("deploy.colour")}
if coloured:
    import re
    tags = json.load(open(sys.argv[4])).get("services", {}) if sys.argv[4:] else {}
    colour = lambda n: coloured[n]["labels"]["deploy.colour"]
    row("a coloured service is <name>-blue or <name>-green, as its label says", "",
        " ".join(n for n in coloured if colour(n) not in ("blue", "green") or not n.endswith("-" + colour(n))))
    row("every coloured service has its twin in the other colour", "",
        " ".join(n for n in coloured if n.rsplit("-", 1)[0] + "-" + ("green" if colour(n) == "blue" else "blue") not in coloured))
    for c in ("blue", "green"):
        ups = [(n, coloured[n]["labels"]["deploy.upstream"]) for n in coloured
               if colour(n) == c and coloured[n]["labels"].get("deploy.upstream")]
        row(f"one {c} service names the edge's upstream with deploy.upstream", "1", str(len(ups)))
        for n, up in ups:
            aliases = ((coloured[n].get("networks") or {}).get("edge") or {}).get("aliases") or []
            row(f"{n}: deploy.upstream {up} is <alias>:<port>, with an alias it has on the edge", "yes",
                "yes" if re.fullmatch(r"[a-z0-9][a-z0-9-]*:[0-9]{1,5}", up) and up.split(":")[0] in aliases else "no")
    row("no coloured service publishes a port, which both colours would claim", "",
        " ".join(n for n in coloured if coloured[n].get("ports")))
    row("each colour runs on its own tag alone, so deploying the other leaves it as it is", "",
        " ".join(n for n in coloured if n in tags and any(t in json.dumps(tags[n]) for t in
                 ("stack-test-once", "stack-test-green" if colour(n) == "blue" else "stack-test-blue"))))
PY
# The file once more with a tag of its own for each colour and for what runs once, so that a
# colour reading another's tag, or IMAGE_TAG alone, shows: a deploy of the other colour would
# recreate it while it takes the requests.
env "${stand_ins[@]}" IMAGE_TAG=stack-test-once IMAGE_TAG_BLUE=stack-test-blue IMAGE_TAG_GREEN=stack-test-green \
  "$DOCKER" compose -f deploy/ovh/compose.yaml --profile '*' config --format json > "$T/tags.json" 2>/dev/null
while IFS=$'\x1f' read -r label expected actual; do check "$label" "$expected" "$actual"; done < <(
  python3 "$T/compose-checks.py" "$T/config.json" "$SERVICE" "$own" "$T/tags.json"
)
# A colour's upstream on the edge, and its services, for the snippets and the switch below.
colour_of() { # colour_of <blue|green> <upstream|services>
  python3 -c '
import json, sys
c, what = sys.argv[2], sys.argv[3]
picked = [(n, (s.get("labels") or {})) for n, s in json.load(open(sys.argv[1])).get("services", {}).items()
          if (s.get("labels") or {}).get("deploy.colour") == c]
print(" ".join(l.get("deploy.upstream", "") for n, l in picked if l.get("deploy.upstream")) if what == "upstream"
      else " ".join(n for n, l in picked))' "$T/config.json" "$1" "$2"
}
blue_up=$(colour_of blue upstream) green_up=$(colour_of green upstream) blue_svcs=$(colour_of blue services)

# Each scheduled job as gcloud-ovh-migrate's CONTRACT.md §7 has it: a <service>-<job> unit
# that names its service and job for the collector, stops at a time limit, runs under hc-wrap
# with its Healthchecks slug, and a timer on a stated time zone.
if [ -d deploy/ovh/systemd ]; then
  cat > "$T/unit-checks.py" <<'PY'
import os, re, sys
service, d = sys.argv[1], sys.argv[2]
def row(label, expected, actual): print(f"{label}\x1f{expected}\x1f{actual}")
units = sorted(f for f in os.listdir(d) if f.endswith((".service", ".timer")))
for f in units:
    m = re.fullmatch(re.escape(service) + r"-([a-z0-9][a-z0-9-]*)\.(service|timer)", f)
    row(f"{f}: named {service}-<job>", "yes", "yes" if m else "no")
    if not m:
        continue
    job, kind = m.groups()
    text = open(os.path.join(d, f)).read()
    keys = [l.split("=", 1) for l in text.splitlines() if "=" in l and not l.lstrip().startswith(("#", ";"))]
    values = lambda k: [v.strip() for key, v in keys if key.strip() == k]
    if kind == "service":
        row(f"{f}: names its service and job for the collector", f"SERVICE={service} JOB={job}",
            " ".join(sorted(values("LogExtraFields"), reverse=True)))
        row(f"{f}: stops at a time limit", "yes",
            "yes" if [v for v in values("TimeoutStartSec") if v not in ("", "0", "infinity")] else "no")
        row(f"{f}: pings its own Healthchecks check", f"HEALTHCHECKS_SLUG={service}-{job}",
            " ".join(v for v in values("Environment") if v.startswith("HEALTHCHECKS_SLUG=")))
        row(f"{f}: runs under hc-wrap", "yes", "yes" if any("hc-wrap" in v for v in values("ExecStart")) else "no")
        row(f"{f}: has its timer", "yes", "yes" if f"{service}-{job}.timer" in units else "no")
    else:
        row(f"{f}: fires on a stated time zone", "yes",
            "yes" if values("OnCalendar") and all(re.search(r" (UTC|[A-Z][A-Za-z_]+/[A-Za-z_/]+)$", v) for v in values("OnCalendar")) else "no")
PY
  while IFS=$'\x1f' read -r label expected actual; do check "$label" "$expected" "$actual"; done < <(
    python3 "$T/unit-checks.py" "$SERVICE" deploy/ovh/systemd
  )
fi

aliases=$(python3 -c '
import json, sys
cfg = json.load(open(sys.argv[1]))
for s in cfg.get("services", {}).values():
    for a in ((s.get("networks") or {}).get("edge") or {}).get("aliases") or []: print(a)' "$T/config.json")
names=()
for f in deploy/ovh/*.caddyfile; do
  [ -f "$f" ] || continue
  snip=$(basename "$f")
  blocks=$(grep -cE '^[^#[:space:]][^{]*\{[[:space:]]*$' "$f" || true)
  check "$snip: every site block imports access_log" "$blocks" "$(grep -cE '^[[:space:]]+import access_log[[:space:]]*$' "$f" || true)"
  check "$snip: every site block imports security_headers" "$blocks" "$(grep -cE '^[[:space:]]+import security_headers[[:space:]]*$' "$f" || true)"
  while IFS= read -r addr; do
    case "$addr" in *.*) names+=("$addr") ;; *) check "$snip: '$addr' names a host, not everyone" yes no ;; esac
  done < <(grep -E '^[^#[:space:]][^{]*\{[[:space:]]*$' "$f" | sed -E 's/[[:space:]]*\{[[:space:]]*$//' | tr ',' '\n' \
             | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//; s#^https?://##')
  while IFS= read -r upstream; do
    check "$snip: it proxies to $upstream, a name this stack has on the edge" yes \
      "$(grep -qx "${upstream%%:*}" <<<"$aliases" && echo yes || echo no)"
  done < <(sed -nE 's/^[[:space:]]+import proxy[[:space:]]+([^[:space:]]+).*/\1/p' "$f")
  live=$(grep -cE "^[[:space:]]+import \.\./live/$SERVICE\.caddyfile[[:space:]]*$" "$f" || true)
  if [ -n "$blue_up" ]; then
    check "$snip: every site block reaches the stack in colours through its live upstream, ../live/$SERVICE.caddyfile" "$blocks" "$live"
    check "$snip: and none past it, to one colour's alias" 0 "$(grep -cE '^[[:space:]]+import proxy[[:space:]]' "$f" || true)"
  else
    check "$snip: imports a live upstream only for a stack in colours" 0 "$live"
  fi
done
check "the stack serves at least one name" yes "$([ ${#names[@]} -gt 0 ] && echo yes || echo no)"

# ---- the stack, running ---------------------------------------------------------
# Each image this repo builds, from this checkout, under the reference compose will ask for.
for spec in ${IMAGES:-}; do
  # name=context:dockerfile[:target], as deploy/ovh/service.env says.
  name=${spec%%=*} rest=${spec#*=}; context=${rest%%:*} rest=${rest#*:}
  dockerfile=${rest%%:*} target=; [ "$rest" = "$dockerfile" ] || target=${rest#*:}
  ref=$(grep -E "/$name:stack-test$" <<<"$images" | head -1)
  [ -n "$ref" ] || { check "image $name is one the compose file runs" yes no; continue; }
  from="$context/$dockerfile${target:+, stage $target}"
  if "$DOCKER" build -q -t "$ref" -f "$context/$dockerfile" ${target:+--target "$target"} "$context" >/dev/null 2>"$T/build.err"; then
    built+=("$ref"); check "image $name builds from $from" 0 0
  else
    tail -5 "$T/build.err" >&2; check "image $name builds from $from" 0 1
  fi
done

# A container compose gives no healthcheck needs one in its image: ci-deploy's wait counts on
# it to tell a release that serves from one that only started (CONTRACT.md §6).
while IFS=$'\t' read -r svc ref; do
  test=$("$DOCKER" image inspect -f '{{if .Config.Healthcheck}}{{join .Config.Healthcheck.Test " "}}{{end}}' "$ref" 2>/dev/null)
  check "$svc: its image has the healthcheck compose leaves out" yes \
    "$([ -n "$test" ] && [ "$test" != NONE ] && echo yes || echo no)"
done < <(python3 -c '
import json, sys
built = set(sys.argv[2:])
for n, s in json.load(open(sys.argv[1])).get("services", {}).items():
    if not s.get("profiles") and not (s.get("healthcheck") or {}).get("test") and s.get("image") in built:
        print("%s\t%s" % (n, s["image"]))' "$T/config.json" ${built[@]+"${built[@]}"})

"$DOCKER" network create "$NET" >/dev/null
# What the stack needs before it can come up — a database migrated and seeded — is the
# service's own to say, in deploy/ovh/stack-test-setup.sh, run with compose pointed at this
# stack and its stand-ins: a plain `docker compose …` there reaches it.
if [ -e deploy/ovh/stack-test-setup.sh ]; then
  env "${stand_ins[@]}" COMPOSE_PROJECT_NAME="$PROJECT" \
    COMPOSE_FILE="deploy/ovh/compose.yaml$([ ! -f deploy/ovh/stack-test.yaml ] || echo :deploy/ovh/stack-test.yaml):$T/override.yaml" \
    ./deploy/ovh/stack-test-setup.sh >"$T/setup.log" 2>&1
  code=$?
  [ "$code" -eq 0 ] || tail -15 "$T/setup.log" >&2
  check "deploy/ovh/stack-test-setup.sh prepares the stack" 0 "$code"
fi
compose up -d --wait --wait-timeout 240 >"$T/up.log" 2>&1
code=$?
[ "$code" -eq 0 ] || { tail -15 "$T/up.log" >&2; compose ps -a >&2; }
check "the stack comes up healthy" 0 "$code"

# The edge as the host runs it — its Caddyfile, Caddy's internal CA in place of Let's Encrypt,
# and this service's snippets without their tls lines — on the stack's edge network.
mkdir -p "$T/edge/sites"
python3 - "$PLATFORM/edge/Caddyfile" "$T/edge/Caddyfile" <<'PY'
import sys
s = open(sys.argv[1]).read()
i = s.index("{\n") + 2
open(sys.argv[2], "w").write(s[:i] + "\tlocal_certs\n" + s[i:])
PY
cp -R "$PLATFORM/edge/cloudflare" "$T/edge/"
# A stack in colours starts on blue, as a first deploy leaves it.
if [ -n "$blue_up" ]; then mkdir -p "$T/edge/live" && printf 'import proxy %s\n' "$blue_up" > "$T/edge/live/$SERVICE.caddyfile"; fi
for f in deploy/ovh/*.caddyfile; do
  python3 -c 'import re, sys; s = open(sys.argv[1]).read(); s = re.sub(r"\n[ \t]*tls \{[^}]*\}", "", s); s = re.sub(r"\n[ \t]*tls [^\n]*", "", s); s = re.sub(r"\n[ \t]*import cloudflare_only[^\n]*", "", s); open(sys.argv[2], "w").write(s)' \
    "$f" "$T/edge/sites/$(basename "$f")"
done
P443=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
"$DOCKER" run -d --name "$EDGE" --network "$NET" -p "127.0.0.1:$P443:443" -v "$T/edge:/etc/caddy:ro" \
  -e EDGE_HOSTNAME=edge.test "$CADDY_IMAGE" >/dev/null
for _ in $(seq 1 40); do "$DOCKER" exec "$EDGE" wget -q -O /dev/null http://127.0.0.1:2020/metrics 2>/dev/null && break; sleep 0.5; done
check "the edge starts with this service's snippets" running "$("$DOCKER" inspect -f '{{.State.Status}}' "$EDGE" 2>/dev/null)"

for name in "${names[@]}"; do
  out=$(curl -sk --max-time 30 -D - -o /dev/null --connect-to "$name:443:127.0.0.1:$P443" "https://$name/" | tr -d '\r')
  status=$(head -1 <<<"$out" | awk '{print $2}')
  check "$name: answered by the stack through the edge" yes \
    "$([[ $status =~ ^[234][0-9][0-9]$ ]] && echo yes || echo "no (${status:-no answer})")"
  check "$name: with the edge's security headers" yes \
    "$(grep -qi '^strict-transport-security:' <<<"$out" && echo yes || echo no)"
done

# ---- the switch, as a deploy makes it --------------------------------------------
# Green takes the edge with one reload while requests keep coming, and blue then stops, as
# `retire` stops it. Not one of them may fail or wait on it: that is the point of the colours.
if [ -n "$green_up" ] && [ ${#names[@]} -gt 0 ]; then
  rm -f "$T/stop"
  ( while [ ! -e "$T/stop" ]; do
      curl -sk --max-time 30 -o /dev/null -w '%{http_code} %{time_total}\n' \
        --connect-to "${names[0]}:443:127.0.0.1:$P443" "https://${names[0]}/"
      sleep 0.1
    done > "$T/switch.log" ) &
  loop=$!
  sleep 1
  printf 'import proxy %s\n' "$green_up" > "$T/edge/live/$SERVICE.caddyfile"
  "$DOCKER" exec "$EDGE" caddy reload --config /etc/caddy/Caddyfile >/dev/null 2>&1
  check "the edge takes green with one reload" 0 "$?"
  sleep 1
  # shellcheck disable=SC2086 # one word per service
  compose stop $blue_svcs >/dev/null 2>&1
  sleep 2
  touch "$T/stop"; wait "$loop"
  read -r sent failed slowest < <(python3 -c '
import sys
rows = [l.split() for l in open(sys.argv[1]) if l.strip()]
print(len(rows), sum(1 for r in rows if r[0] == "000" or r[0] >= "500"), int(max((float(r[1]) for r in rows), default=0) * 1000))' "$T/switch.log")
  check "not one of $sent requests failed while the edge moved to green and blue stopped" 0 "$failed"
  check "and none waited on it: the slowest took ${slowest} ms" yes "$([ "$slowest" -lt 1000 ] && echo yes || echo no)"
  out=$(curl -sk --max-time 30 -o /dev/null -w '%{http_code}' --connect-to "${names[0]}:443:127.0.0.1:$P443" "https://${names[0]}/")
  check "${names[0]}: answered by green alone" yes "$([[ $out =~ ^[234][0-9][0-9]$ ]] && echo yes || echo "no ($out)")"
fi

# ---- what the stack's own images write ------------------------------------------
# Every line from a container running an image this repo builds, from start to the requests
# above, is one JSON object with time, level and msg (gcloud-ovh-migrate's CONTRACT.md §9).
# What a third-party image writes is the host's collector's to read, not this test's.
own=$(python3 -c '
import json, sys
built = set(sys.argv[2:])
for name, s in json.load(open(sys.argv[1])).get("services", {}).items():
    if s.get("image") in built and not s.get("profiles"):
        print(name)' "$T/config.json" ${built[@]+"${built[@]}"})
cat > "$T/lines.py" <<'PY'
import json, re, sys
TIME = re.compile(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3,9}(Z|\+00:00)")
lines = [l.rstrip("\n") for l in open(sys.argv[1], errors="replace") if l.strip()]
bad = []
for line in lines:
    try:
        o = json.loads(line)
    except ValueError:
        o = None
    if not isinstance(o, dict):
        bad.append(("not a JSON object", line))
        continue
    why = [k for k, ok in (("time", TIME.fullmatch(str(o.get("time", "")))),
                           ("level", o.get("level") in ("debug", "info", "warn", "error")),
                           ("msg", isinstance(o.get("msg"), str) and o["msg"])) if not ok]
    if why:
        bad.append(("no valid " + ", ".join(why), line))
if bad:
    print("%d of %d lines; the first, %s: %s" % (len(bad), len(lines), bad[0][0], bad[0][1][:100]), end="")
PY
for svc in $own; do
  compose logs --no-color --no-log-prefix "$svc" > "$T/$svc.log" 2>/dev/null
  verdict=$(python3 "$T/lines.py" "$T/$svc.log")
  # A container service.env names in LOG_FORMAT_PENDING is on its way there, under an issue of
  # its own: said, not failed, until its lines pass and it can leave the list.
  base=$svc; case $svc in *-blue|*-green) base=${svc%-*} ;; esac
  if [[ " ${LOG_FORMAT_PENDING:-} " == *" $svc "* || " ${LOG_FORMAT_PENDING:-} " == *" $base "* ]]; then
    if [ -n "$verdict" ]; then
      echo "skip $svc: not yet in CONTRACT.md §9's format, as LOG_FORMAT_PENDING says ($verdict)"
    elif ! grep -q . "$T/$svc.log"; then
      # Off for the test, as a sync client with no account to sign in to is: no line, no verdict.
      echo "skip $svc: it wrote nothing here, so its format is not known; LOG_FORMAT_PENDING keeps it"
    else
      echo "skip $svc: its lines already pass; take it off LOG_FORMAT_PENDING in deploy/ovh/service.env"
    fi
    continue
  fi
  check "$svc: every line it writes is one JSON object with time, level and msg" "" "$verdict"
done

echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
