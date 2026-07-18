#!/usr/bin/env bash
# Unit test for the RAIL container's audio preflight warnings — the
# `audio_preflight` function in docker/entrypoint-rail.sh.
#
# No Docker needed. We source the entrypoint (its sourced-guard stops it before
# it boots Weston) to get the *real* function, then drive it through all four
# states:
#   libpulse {present, absent}  ×  bridge {reachable, unreachable}
# `ldconfig` and `timeout` are shimmed on PATH so the outcome is deterministic on
# any machine (macOS ships neither by default); the reachability probe runs
# against a real localhost TCP listener (reachable) and a known-free port
# (unreachable). Run:  scripts/test-audio-preflight.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- shims: make ldconfig / timeout deterministic regardless of the host -------
SHIMDIR="$(mktemp -d)"
# timeout shim: drop the duration arg, exec the rest. Localhost probes below
# return immediately (accept or refuse), so no real timeout behaviour is needed.
cat >"$SHIMDIR/timeout" <<'EOF'
#!/usr/bin/env bash
shift
exec "$@"
EOF
chmod +x "$SHIMDIR/timeout"
export PATH="$SHIMDIR:$PATH"

set_libpulse() { # present|absent — control what `ldconfig -p` reports
    if [ "$1" = present ]; then
        cat >"$SHIMDIR/ldconfig" <<'EOF'
#!/usr/bin/env bash
echo "	libpulse.so.0 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libpulse.so.0"
EOF
    else
        cat >"$SHIMDIR/ldconfig" <<'EOF'
#!/usr/bin/env bash
echo "	libc.so.6 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libc.so.6"
EOF
    fi
    chmod +x "$SHIMDIR/ldconfig"
}

# --- a real localhost listener for the "reachable" case -----------------------
PORTFILE="$(mktemp)"
python3 - "$PORTFILE" <<'PY' &
import socket, sys, time
s = socket.socket(); s.bind(("127.0.0.1", 0)); s.listen(16)
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
time.sleep(60)
PY
LISTENER_PID=$!
for _ in $(seq 1 50); do [ -s "$PORTFILE" ] && break; sleep 0.1; done
LIVE_PORT="$(cat "$PORTFILE")"
# a port nothing listens on → connection refused
DEAD_PORT="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"

cleanup() { kill "$LISTENER_PID" 2>/dev/null; rm -rf "$SHIMDIR" "$PORTFILE"; }
trap cleanup EXIT

# Pull in the real audio_preflight (guard stops the entrypoint before bring-up).
# shellcheck source=../docker/entrypoint-rail.sh
source "$ROOT/docker/entrypoint-rail.sh"
if ! declare -F audio_preflight >/dev/null; then
    echo "FAIL: sourcing docker/entrypoint-rail.sh did not define audio_preflight"
    exit 1
fi

# --- assertions ---------------------------------------------------------------
fails=0
MUTED_RE='will play MUTED'

assert() { # <desc> <should-contain|should-not-contain> <needle> <haystack>
    local desc="$1" mode="$2" needle="$3" hay="$4"
    if [ "$mode" = has ]; then
        if grep -q -- "$needle" <<<"$hay"; then echo "  ok: $desc"
        else echo "  FAIL: $desc — expected to see: $needle"; echo "$hay" | sed 's/^/      | /'; fails=$((fails+1)); fi
    else
        if grep -q -- "$needle" <<<"$hay"; then echo "  FAIL: $desc — did NOT expect: $needle"; echo "$hay" | sed 's/^/      | /'; fails=$((fails+1))
        else echo "  ok: $desc"; fi
    fi
}

echo "case 1: libpulse present, bridge reachable → no warnings"
set_libpulse present
out="$(audio_preflight "tcp:127.0.0.1:${LIVE_PORT}")"
assert "no libpulse warning" not "libpulse not found" "$out"
assert "reports reachable"   has "audio bridge reachable at 127.0.0.1:${LIVE_PORT}" "$out"
assert "nothing muted"       not "$MUTED_RE" "$out"

echo "case 2: libpulse present, bridge unreachable → unreachable warning only"
out="$(audio_preflight "tcp:127.0.0.1:${DEAD_PORT}")"
assert "no libpulse warning"   not "libpulse not found" "$out"
assert "warns unreachable"     has "is UNREACHABLE" "$out"
assert "points at the fix"     has "scripts/pulseaudio-mac.sh" "$out"

echo "case 3: libpulse absent, bridge reachable → libpulse warning only"
set_libpulse absent
out="$(audio_preflight "tcp:127.0.0.1:${LIVE_PORT}")"
assert "warns libpulse missing" has "libpulse not found" "$out"
assert "points at rebuild"      has "docker compose --profile rail build" "$out"
assert "not marked unreachable" not "is UNREACHABLE" "$out"
assert "still reports reachable" has "audio bridge reachable" "$out"

echo "case 4: libpulse absent, bridge unreachable → both warnings"
out="$(audio_preflight "tcp:127.0.0.1:${DEAD_PORT}")"
assert "warns libpulse missing" has "libpulse not found" "$out"
assert "warns unreachable"      has "is UNREACHABLE" "$out"

echo "case 5: empty PULSE_SERVER → no reachability line (malformed addr guarded)"
set_libpulse present
out="$(audio_preflight "")"
assert "no reachable line" not "audio bridge reachable" "$out"
assert "no unreachable line" not "is UNREACHABLE" "$out"

echo
if [ "$fails" -eq 0 ]; then echo "PASS: all audio preflight cases"; else echo "FAILED: $fails assertion(s)"; fi
exit "$fails"
