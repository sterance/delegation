#!/bin/sh
set -e

SERVER="${DELEGATION_SERVER:-http://localhost:7070}"
ARGS="--server $SERVER"

# Only needed on first run for a given identity volume — once enrolled,
# the identity file already exists in the mounted volume, so this is
# harmless to leave set in your .env file indefinitely (the pairing code
# is single-use server-side and simply won't be read again).
if [ -n "$PAIRING_CODE" ]; then
  ARGS="$ARGS --pairing-code $PAIRING_CODE"
fi

exec /usr/local/bin/delegation-agent $ARGS
