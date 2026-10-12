#!/usr/bin/env bash
# What a release has to pass on the host before it is kept. scripts/ovh-release.sh runs this
# once the deploy is healthy, and when it fails puts the release before back (gcloud-ovh-migrate's
# CONTRACT.md §6). The service's own: started from the template asking the public name for its
# health, and meant to grow into what the service cannot do without.
set -euo pipefail
url=https://wdig.itsnotezra.com/api/v1/health
curl -fsS -o /dev/null --max-time 20 --retry 3 --retry-delay 5 --retry-all-errors "$url"
echo "ok $url answers"
