#!/usr/bin/env bash
# Edit or inspect a SOPS-encrypted file, with sops in a container so nothing is installed
# on this Mac. Needs the age private key locally to decrypt; see SECRETS.md.
#
# The editor runs inside the container, so it is the container's vi, not this Mac's
# $EDITOR — an EDITOR naming a Mac app would not exist in there. SECRETS_EDITOR=vim works
# too. A file that does not exist yet is created, encrypted on save to the recipients
# .sops.yaml names for its path.
#
# `set` is for one value, typically a token copied from a web page: it is read from a
# hidden prompt and never shown, never written to a file in the clear, and never put on a
# command line. sops edit, by contrast, keeps a plaintext copy in the container while vi
# has it open.
#
#   secrets.sh edit  path/to/file.enc.env
#   secrets.sh set   path/to/file.enc.env KEY  (value from a hidden prompt or stdin)
#   secrets.sh show  path/to/file.enc.env      (prints keys only, never values)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
: "${DOCKER:=docker}" "${SOPS_IMAGE:=ghcr.io/getsops/sops:v3.9.4-alpine}"

AGE_KEY=${SOPS_AGE_KEY_FILE:-$HOME/.config/sops/age/keys.txt}
cmd=${1:-}; file=${2:-}
usage() { echo "usage: $0 {edit|show} <file.enc.env> | set <file.enc.env> <KEY>" >&2; exit 2; }
# Either check failing means usage; nothing after `usage` runs, since it exits.
# shellcheck disable=SC2015
[ -n "$cmd" ] && [ -n "$file" ] || usage

case "$cmd" in
  edit)
    [ -f "$AGE_KEY" ] || { echo "x no age key at $AGE_KEY; see SECRETS.md" >&2; exit 1; }
    exec "$DOCKER" run --rm -it \
      -v "$PWD:/w" -w /w \
      -v "$AGE_KEY:/age.txt:ro" -e SOPS_AGE_KEY_FILE=/age.txt \
      -e EDITOR="${SECRETS_EDITOR:-vi}" \
      "$SOPS_IMAGE" edit "$file"
    ;;
  set)
    key=${3:-}
    [[ $key =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || usage
    [ -f "$AGE_KEY" ] || { echo "x no age key at $AGE_KEY; see SECRETS.md" >&2; exit 1; }
    value=
    if [ -t 0 ]; then
      printf 'Paste %s (it will not be shown), then Enter: ' "$key" >&2
      IFS= read -rs value || true; echo >&2
    else
      IFS= read -r value || true
    fi
    [ -n "$value" ] || { echo "x nothing entered; $file is unchanged" >&2; exit 1; }

    # The value travels on stdin: not a file, not a command line, and not the container's
    # environment, which docker inspect would show. Inside, one pipe decrypts the current
    # file, drops the key's old line, appends the new one and encrypts the lot under the
    # rule .sops.yaml gives this path. The result lands in the container's /tmp first, so
    # a failure anywhere leaves the file as it was.
    # The script in single quotes runs inside the container, which expands it.
    # shellcheck disable=SC2016
    printf '%s\n' "$value" | "$DOCKER" run --rm -i \
      -v "$PWD:/w" -w /w \
      -v "$AGE_KEY:/age.txt:ro" -e SOPS_AGE_KEY_FILE=/age.txt \
      --entrypoint sh "$SOPS_IMAGE" -c '
        set -eo pipefail
        IFS= read -r v
        { if [ -f "$1" ]; then
            sops decrypt "$1" | while IFS= read -r l; do
              case $l in "$2"=*) ;; *) printf "%s\n" "$l" ;; esac
            done
          fi
          printf "%s=%s\n" "$2" "$v"
        } | sops encrypt --filename-override "$1" --input-type dotenv --output-type dotenv /dev/stdin > /tmp/new
        cat /tmp/new > "$1"' -- "$file" "$key"
    echo "ok $key set in $file" >&2
    ;;
  show)
    # Variable names only. Printing a decrypted value to a terminal puts it in scrollback
    # and in any terminal log, which is exactly what SECRETS.md is avoiding.
    grep -oE '^[A-Za-z_][A-Za-z0-9_]*' "$file" | grep -v '^sops_' | sort -u
    ;;
  *)
    usage
    ;;
esac
