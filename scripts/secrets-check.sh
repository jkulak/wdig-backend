#!/usr/bin/env bash
# Refuse the things that must never reach a commit. This is the "nothing secret in the
# diff — checked, not assumed" line of the Definition of Done, made mechanical.
#
# This file is excluded from its own scan: it necessarily contains the patterns it looks
# for, and an earlier version reported itself and nothing else.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

SELF=scripts/secrets-check.sh
fail=0
note() { echo "x $1"; fail=1; }

# Tracked and staged files plus untracked ones that are not ignored: a file about to be
# added is exactly the one worth checking. Ignored files are not — .env is meant to exist.
files=()
while IFS= read -r f; do
  [ "$f" = "$SELF" ] && continue
  [ -f "$f" ] || continue
  files+=("$f")
done < <(git ls-files --cached --others --exclude-standard)

if [ ${#files[@]} -eq 0 ]; then echo "ok secrets: nothing to check"; exit 0; fi

# A private key or a .env by name, whatever .gitignore says — a force-add bypasses it.
for f in "${files[@]}"; do
  case "$f" in
    .env|*/.env|*.key|*.age) note "$f is tracked and must not be" ;;
  esac
done

# A file named like a credential, or sitting in a directory that is. A password pasted into
# `ovh-pass-secret` matches no token pattern below, and neither do OVH's S3 keys in
# `secrets/user-…txt`; both turned up untracked in this repo, one `git add -A` away from
# GitHub. Every component of the path is checked, not only the file name. Documentation
# and scripts about secrets (.md, .sh), sops files, and code — a password reset, a test of
# one, as bankdata's tests/test_first_password.py — are fine: a credential is not kept in a
# source file by that name.
for f in "${files[@]}"; do
  base=$(basename "$f" | tr '[:upper:]' '[:lower:]')
  case "$base" in *.md|*.sh|*.enc.env|*.py|*.php|*.phtml|*.js|*.ts|*.tsx|*.go|*.rs|*.swift|*.sql|*.html) continue ;; esac
  if printf '%s' "$f" | tr '[:upper:]' '[:lower:]' | tr '/' '\n' \
     | grep -qE '(^|[^a-z])(pass|passwd|password|passwords|secret|secrets|credential|credentials|token|tokens)([^a-z]|$)'; then
    note "$f is named like a credential, or sits in a directory that is; keep it out of the repo"
  fi
done

# Every *.enc.env must be encrypted all the way through. A file that has sops metadata
# but plaintext values is what a bad creation rule produces, and it looks encrypted at a
# glance. Every KEY=value line that is not sops's own metadata must hold ENC[...].
for f in "${files[@]}"; do
  case "$f" in *.enc.env) ;; *) continue;; esac
  grep -q '^sops_mac=' "$f" || { note "$f has no sops_mac: it was never encrypted by sops"; continue; }
  plain=$(grep -E '^[A-Za-z_][A-Za-z0-9_]*=' "$f" | grep -vE '^sops_' | grep -vE '^[A-Za-z_][A-Za-z0-9_]*=ENC\[AES256_GCM,' | cut -d= -f1 || true)
  [ -z "$plain" ] || note "$f has plaintext values for: $(echo "$plain" | tr '\n' ' ')"
done

# Every *.enc.env must open with exactly the keys .sops.yaml gives its path. One key more
# reads what it should not, like the host key on the Grafana token; one fewer can lose the
# file, like a backup secret the personal key cannot open once the host is gone. Which keys
# a path gets is asked of sops itself: a probe encrypted under the file's own path goes
# through the same creation rule, so this cannot drift from how sops reads .sops.yaml.
enc=()
for f in "${files[@]}"; do case "$f" in *.enc.env) enc+=("$f") ;; esac; done
if [ ${#enc[@]} -gt 0 ]; then
  # The script in single quotes runs inside the container, which expands it.
  # shellcheck disable=SC2016
  expected=$("${DOCKER:-docker}" run --rm -v "$PWD:/w:ro" -w /w --entrypoint sh \
    "${SOPS_IMAGE:-ghcr.io/getsops/sops:v3.9.4-alpine}" -c '
      for f in "$@"; do
        keys=$(printf "PROBE=x\n" \
          | sops encrypt --filename-override "$f" --input-type dotenv --output-type dotenv /dev/stdin 2>/dev/null \
          | sed -n "s/^sops_age__list_[0-9]*__map_recipient=//p" | sort | tr "\n" " ")
        printf "%s\t%s\n" "$f" "${keys:-none}"
      done' -- "${enc[@]}") \
    || { note "could not ask sops which keys each file should open with; is Docker running?"; expected=; }
  lines() { tr ' ' '\n' <<<"$1" | sed '/^$/d'; }
  while IFS=$'\t' read -r f want; do
    [ -n "$f" ] || continue
    if [ "$want" = none ]; then note "$f: no creation rule in .sops.yaml covers it"; continue; fi
    have=$(sed -n 's/^sops_age__list_[0-9]*__map_recipient=//p' "$f" | sort | tr '\n' ' ')
    extra=$(comm -13 <(lines "$want") <(lines "$have") | tr '\n' ' ')
    missing=$(comm -23 <(lines "$want") <(lines "$have") | tr '\n' ' ')
    [ -z "$extra" ] || note "$f also opens with a key .sops.yaml does not give it: $extra"
    [ -z "$missing" ] || note "$f does not open with a key .sops.yaml requires: $missing"
  done <<<"$expected"
  # Only age is in use. A pgp, KMS or Vault entry would be a key nobody chose here.
  for f in "${enc[@]}"; do
    if grep -qE '^sops_(pgp|kms|gcp_kms|azure_kv|hc_vault)' "$f"; then note "$f opens with a key that is not age"; fi
  done
fi

hits() { grep -lIE "$1" "${files[@]}" 2>/dev/null || true; }
look() { # look <description> <regex>
  local found; found=$(hits "$2")
  [ -z "$found" ] || note "$1 appears in: $(echo "$found" | tr '\n' ' ')"
}

look "an age private key"                 'AGE-SECRET-KEY-1'
look "a Grafana service-account token"    'glsa_[A-Za-z0-9]{16,}'
look "a Healthchecks ping URL"            'hc-ping\.com/[0-9a-f]{8}-[0-9a-f]{4}'
look "a GitHub token"                     'gh[pousr]_[A-Za-z0-9]{20,}'
look "a private key block"                'BEGIN (RSA|OPENSSH|EC|PGP) PRIVATE KEY'

[ $fail -eq 0 ] || { echo "secrets-check failed"; exit 1; }
echo "ok secrets: nothing forbidden in ${#files[@]} file(s)"
