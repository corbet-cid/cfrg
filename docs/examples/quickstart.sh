#!/usr/bin/env bash
# SPDX-License-Identifier: LGPL-3.0-only WITH LGPL-3.0-linking-exception
#
# The quick start of docs/quickstart.md as one script: a signed `release` event is delivered to `cfrg-serve`, which
# uploads the release of a throwaway Forgejo repository to a throwaway GitLab project. Both are created here and
# deleted on exit (the deletion is verified).
#
# Needs on PATH: cfrg-serve, curl, git, jq, openssl. Environment (no default):
#   FORGE_URL        Forgejo origin, e.g. https://forge.example.org
#   FORGE_TOKEN      Forgejo token that may create and delete repositories (write:repository, write:user)
#   GITLAB_TOKEN     GitLab token with scope api
#   GITLAB_PATH      the GitLab user or top-level group that receives the throwaway project
# Optional: GITLAB_API (default https://gitlab.com/api/v4), PORT (default 18080), WORK (default a fresh temp directory).
set -euo pipefail

: "${FORGE_URL:?}" "${FORGE_TOKEN:?}" "${GITLAB_TOKEN:?}" "${GITLAB_PATH:?}"
GITLAB_API="${GITLAB_API:-https://gitlab.com/api/v4}"
PORT="${PORT:-18080}"
WORK="${WORK:-$(mktemp -d)}"
NAME="cfrg-qs-throwaway-$(date +%s)"
TAG="v0.0.1"
fapi() { curl -fsS -H "Authorization: token $FORGE_TOKEN" -H 'Content-Type: application/json' "$@"; }
REPO="$(fapi "$FORGE_URL/api/v1/user" | jq -r .login)/$NAME" # the throwaway lives in the token's own account
gapi() { curl -fsS -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "$@"; }
pid=""

cleanup() {
  set +e
  [ -n "$pid" ] && kill "$pid" 2>/dev/null
  curl -s -o /dev/null -X DELETE -H "Authorization: token $FORGE_TOKEN" "$FORGE_URL/api/v1/repos/$REPO"
  # gitlab.com schedules a deletion (and renames the project); a second call with the new path removes it for good.
  gid=$(gapi "$GITLAB_API/projects/$(jq -rn --arg p "$GITLAB_PATH/$NAME" '$p|@uri')" 2>/dev/null | jq -r '.id // empty')
  if [ -n "$gid" ]; then
    curl -s -o /dev/null -X DELETE -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "$GITLAB_API/projects/$gid"
    sleep 2
    full=$(gapi "$GITLAB_API/projects/$gid" 2>/dev/null | jq -r '.path_with_namespace // empty')
    [ -n "$full" ] && curl -s -o /dev/null -X DELETE -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "$GITLAB_API/projects/$gid?permanently_remove=true&full_path=$(jq -rn --arg p "$full" '$p|@uri')"
  fi
  sleep 3
  f=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: token $FORGE_TOKEN" "$FORGE_URL/api/v1/repos/$REPO")
  g=$([ -n "$gid" ] && curl -s -o /dev/null -w '%{http_code}' -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "$GITLAB_API/projects/$gid")
  echo "cleanup: forgejo repository -> HTTP $f, gitlab project -> HTTP $g (404 = gone)"
}
trap cleanup EXIT

echo "1. throwaway repositories: $REPO on the primary, $GITLAB_PATH/$NAME on the copy"
fapi -X POST "$FORGE_URL/api/v1/user/repos" -d "{\"name\":\"$NAME\",\"auto_init\":true,\"default_branch\":\"main\"}" >/dev/null
# CI is off on the copy from the start: the upload refuses to write while builds are enabled.
gapi -X POST "$GITLAB_API/projects" --data-urlencode "name=$NAME" --data-urlencode "path=$NAME" \
  --data-urlencode "builds_access_level=disabled" --data-urlencode "visibility=public" >/dev/null

echo "2. a release with one file and its checksum on the primary"
mkdir -p "$WORK/asset" && cd "$WORK/asset"
echo "hello from the quick start" >hello.txt
sha256sum hello.txt >SHA256SUMS
fapi -X POST "$FORGE_URL/api/v1/repos/$REPO/releases" \
  -d "{\"tag_name\":\"$TAG\",\"target_commitish\":\"main\",\"name\":\"$TAG\",\"body\":\"quick start\"}" >"$WORK/release.json"
rid=$(jq -r .id "$WORK/release.json")
for f in hello.txt SHA256SUMS; do
  curl -fsS -H "Authorization: token $FORGE_TOKEN" -F "attachment=@$f" "$FORGE_URL/api/v1/repos/$REPO/releases/$rid/assets?name=$f" >/dev/null
done

echo "3. the tag reaches the copy (in production the push mirror does this)"
cd "$WORK"
git -c http.extraHeader="Authorization: token $FORGE_TOKEN" clone -q "$FORGE_URL/$REPO.git" clone
cd clone
git fetch -q --tags
git remote add copy "https://oauth2:$GITLAB_TOKEN@$(echo "$GITLAB_API" | sed 's|^https://||; s|/api/v4$||')/$GITLAB_PATH/$NAME.git"
git push -q copy "HEAD:refs/heads/main" "refs/tags/$TAG"

echo "4. configuration and credentials (one file per key, like a mounted Secret)"
mkdir -p "$WORK/secrets"
key=$(openssl rand -hex 24)
printf %s "$key" >"$WORK/secrets/hmac_forgejo"
printf %s "$FORGE_TOKEN" >"$WORK/secrets/forgejo-token"
printf %s "$GITLAB_TOKEN" >"$WORK/secrets/gitlab-token"
jq -n --arg api "$FORGE_URL/api/v1" --arg web "$FORGE_URL" --arg gl "$GITLAB_API" --arg ns "$GITLAB_PATH" --arg port "$PORT" \
  --arg from "$REPO" --arg to "$GITLAB_PATH/$NAME" '{
    listeners: {internal: {listen: "127.0.0.1", port: ($port|tonumber)}},
    forge_api: $api, forge_web: $web,
    github: {api: "https://api.github.com", uploads: "https://uploads.github.com", orgs: []},
    gitlab: {api: $gl, groups: [$ns]},
    aliases: {gitlab: {($from): $to}}
  }' >"$WORK/config.json"

echo "5. cfrg-serve"
FORGE_BRIDGE_CONFIG="$WORK/config.json" FORGE_BRIDGE_SECRETS="$WORK/secrets" cfrg-serve serve >"$WORK/serve.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.2; done
curl -fs "http://127.0.0.1:$PORT/healthz" >/dev/null || { cat "$WORK/serve.log"; exit 1; }

echo "6. a signed release event (the body Forgejo sends for a published release)"
body=$(jq -cn --arg r "$REPO" --arg t "$TAG" '{action:"published", repository:{full_name:$r}, release:{tag_name:$t, draft:false}}')
sign() { printf %s "$body" | openssl dgst -sha256 -hmac "$1" -hex | awk '{print $NF}'; }
bad=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "$body" -H "X-Forgejo-Signature: $(sign wrong-key)" "http://127.0.0.1:$PORT/hooks/release")
echo "   wrong signature -> HTTP $bad (expected 401)"
good=$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary "$body" -H "X-Forgejo-Signature: $(sign "$key")" \
  -H "X-Forgejo-Delivery: quickstart-1" "http://127.0.0.1:$PORT/hooks/release")
echo "   right signature -> HTTP $good (expected 2xx)"
for _ in $(seq 1 60); do grep -q '"event":"done"' "$WORK/serve.log" && break; sleep 1; done

echo "7. the result on the GitLab copy"
cat "$WORK/serve.log"
gapi "$GITLAB_API/projects/$(jq -rn --arg p "$GITLAB_PATH/$NAME" '$p|@uri')/releases/$TAG/assets/links" | jq -r '.[].name'
echo "   (cleanup runs now)"
