#!/usr/bin/env bash
# Builds the self-hosted image locally and publishes its files as a GitHub release,
# which Dockerfile.coolify downloads.
set -euo pipefail
APP_URL="${APP_URL:-https://mail.marcinszyda.com}"
# The Coolify host is an ARM server.
RUST_TARGET="${RUST_TARGET:-aarch64-unknown-linux-gnu}"
TAG="selfhost-$(date +%Y%m%d-%H%M%S)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

docker build -f Dockerfile.selfhost --target web --build-arg APP_URL="$APP_URL" -t zero-selfhost:web .
docker build -f Dockerfile.selfhost --target api --build-arg RUST_TARGET="$RUST_TARGET" -t zero-selfhost:api .
cid="$(docker create zero-selfhost:api)"
docker cp "$cid:/zero-server" "$WORK/"
docker rm "$cid" >/dev/null
cid="$(docker create zero-selfhost:web)"
docker cp "$cid:/src/apps/mail/build/client" "$WORK/public"
docker rm "$cid" >/dev/null
tar -C "$WORK" -czf "$WORK/zero-mail-selfhost.tar.gz" zero-server public

gh release create "$TAG" "$WORK/zero-mail-selfhost.tar.gz" --repo tarkeztarkez/Zero \
  --target "$(git rev-parse HEAD)" --title "$TAG" --notes "Self-hosted build for $APP_URL" --prerelease
echo "Published $TAG; set RELEASE_TAG=$TAG in Coolify and redeploy."
