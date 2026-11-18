#!/bin/sh
set -eu

CONFIG_DIR="$(dirname "${CONFIG_FILE:-/config/config.yml}")"
MEDIA_DIR="${NINJANVR_MEDIA_DIR:-/media/ninjanvr}"
CACHE_DIR="${XDG_CACHE_HOME:-/tmp/cache}"

mkdir -p "$CACHE_DIR" 2>/dev/null || true

# 1. Already started as non-root (e.g. Kubernetes securityContext.runAsNonRoot / docker --user)
if [ "$(id -u)" != "0" ]; then
  exec /usr/local/bin/ninjanvr "$@"
fi

TARGET_UID="${PUID:-${NINJANVR_UID:-10001}}"
TARGET_GID="${PGID:-${NINJANVR_GID:-10001}}"

can_run_as_unprivileged() {
  # Check existing sensitive config/secret/db files in CONFIG_DIR
  for f in "$CONFIG_DIR/.jwt_secret" "$CONFIG_DIR/config.yml" "$CONFIG_DIR/ninjanvr.db"; do
    if [ -e "$f" ]; then
      if ! setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups test -r "$f" || \
         ! setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups test -w "$f"; then
        return 1
      fi
    fi
  done

  if [ -d "$CONFIG_DIR" ] && ! setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups test -w "$CONFIG_DIR"; then
    return 1
  fi

  for sub in "" "/recordings" "/clips" "/exports"; do
    if [ -d "${MEDIA_DIR}${sub}" ] && ! setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups test -w "${MEDIA_DIR}${sub}"; then
      return 1
    fi
  done

  return 0
}

# If PUID/NINJANVR_UID was explicitly requested, prepare top-level dirs and drop privileges.
if [ -n "${PUID:-}${NINJANVR_UID:-}" ]; then
  mkdir -p "$CONFIG_DIR" "$MEDIA_DIR" "$CACHE_DIR"
  chown "$TARGET_UID:$TARGET_GID" "$CONFIG_DIR" "$MEDIA_DIR" "$CACHE_DIR" 2>/dev/null || true
  exec setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups /usr/local/bin/ninjanvr "$@"
fi

# Fresh install (empty mounts): initialize top-level directories for unprivileged 10001:10001.
if [ -d "$CONFIG_DIR" ] && [ -z "$(ls -A "$CONFIG_DIR" 2>/dev/null || true)" ]; then
  chown "$TARGET_UID:$TARGET_GID" "$CONFIG_DIR" 2>/dev/null || true
fi
if [ -d "$MEDIA_DIR" ] && [ -z "$(ls -A "$MEDIA_DIR" 2>/dev/null || true)" ]; then
  chown "$TARGET_UID:$TARGET_GID" "$MEDIA_DIR" 2>/dev/null || true
fi

if can_run_as_unprivileged; then
  chown "$TARGET_UID:$TARGET_GID" "$CACHE_DIR" /dev/shm 2>/dev/null || true
  exec setpriv --reuid="$TARGET_UID" --regid="$TARGET_GID" --clear-groups /usr/local/bin/ninjanvr "$@"
fi

echo "[ninjanvr-entrypoint] Legacy root-owned volume detected in $CONFIG_DIR or $MEDIA_DIR; continuing as UID 0 for drop-in upgrade compatibility (chown volumes to 10001:10001 or set PUID/PGID to drop privileges)." >&2
exec /usr/local/bin/ninjanvr "$@"
