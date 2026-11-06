#!/bin/sh
# Tests packaging/install.sh without a speaker: fake soundya binaries in a
# temporary root, a fake init script. Covers a good update, an update whose
# daemon crashes (rollback), a damaged package and that user data is kept.
#   sh tests/install-test.sh            (any POSIX sh: dash, busybox sh, bash)
set -u
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
T=$(mktemp -d)
trap 'pkill -f "$T/" 2>/dev/null; rm -rf "$T"' EXIT
FAILS=0
ok() { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; FAILS=$((FAILS + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

export SOUNDYA_INSTALL_DIR=$T/data SOUNDYA_INIT=$T/S99soundya SOUNDYA_RUN=$T/run SOUNDYA_HEALTH_WAIT=8 SOUNDYA_HEALTH_HOLD=1
mkdir -p "$T/run" "$T/data"

# A fake soundya. $1 version, $2 behaviour of «daemon»: good | crash | legacy.
fake_bin() { # dest version behaviour
    cat > "$1" <<EOF
#!/bin/sh
case "\${1:-}" in
    version) echo "soundya $2" ;;
    status) [ -f "$T/run/up" ] && [ "\$(cat "$T/run/up")" = "$2" ] && echo stopped || exit 1 ;;
    daemon)
        case "$3" in
            crash) exit 3 ;;
            good) echo "$2 \$\$" > "$T/run/soundya.ready" ;;
        esac
        echo "$2" > "$T/run/up"
        exec sleep 1000 ;;
    *) echo "soundya $2"; echo "help" ;;
esac
EOF
    chmod 755 "$1"
}
# A fake init script: «start» runs the daemon in the background, «stop» kills it.
fake_init() {
    cat > "$1" <<EOF
#!/bin/sh
case "\$1" in
    start) ( "$T/data/soundya" daemon >/dev/null 2>&1 & ) ;;
    stop) pkill -f "$T/data/soundya daemon" 2>/dev/null; pkill -f "sleep 1000" 2>/dev/null; rm -f "$T/run/up"; sleep 0.3 ;;
esac
EOF
    chmod 755 "$1"
}
package() { # dir version behaviour
    rm -rf "$1" && mkdir -p "$1"
    cp "$ROOT/packaging/install.sh" "$ROOT/packaging/run.sh" "$ROOT/packaging/guard.sh" \
        "$ROOT/packaging/uninstall.sh" "$ROOT/packaging/soundya.env" "$1/"
    fake_bin "$1/soundya" "$2" "$3"
    fake_init "$1/S99soundya"
    echo "$2" > "$1/VERSION"
    (cd "$1" && sha256sum soundya run.sh guard.sh uninstall.sh install.sh soundya.env S99soundya VERSION > SHA256SUMS)
}
field() { sed -n "s/^$1=//p" "$T/data/update-result"; }

echo "1. fresh install"
package "$T/p1" 0.4.40 legacy
sh "$T/p1/install.sh" >"$T/log1" 2>&1
# A legacy (0.4.x) package writes no ready file: a fresh install of one reports «failed», by design.
echo '{"token":"2-1-secret","ring":false}' > "$T/data/config.json"
echo "abc" > "$T/data/client_id"; echo "KEY" > "$T/data/adbkey"
"$T/S99soundya" start; sleep 0.5

echo "2. good update keeps data"
package "$T/p2" 0.5.0 good
sh "$T/p2/install.sh" >"$T/log2" 2>&1
check "status ok" '[ "$(field status)" = ok ]'
check "from 0.4.40 to 0.5.0" '[ "$(field from)" = 0.4.40 ] && [ "$(field to)" = 0.5.0 ]'
check "new binary in place" '[ "$("$T/data/soundya" version)" = "soundya 0.5.0" ]'
check "config untouched" 'grep -q 2-1-secret "$T/data/config.json"'
check "client_id, adbkey untouched" '[ "$(cat "$T/data/client_id")" = abc ] && [ "$(cat "$T/data/adbkey")" = KEY ]'
check "rollback slot holds 0.4.40" '[ "$("$T/data/backup/prev/soundya" version)" = "soundya 0.4.40" ]'
check "data snapshot made" 'ls "$T"/data/backup/data-*-0.4.40/config.json >/dev/null 2>&1'
echo "X=1" >> "$T/data/soundya.env"  # the user edits soundya.env

echo "3. update whose daemon crashes is rolled back"
package "$T/p3" 0.5.1 crash
sh "$T/p3/install.sh" >"$T/log3" 2>&1
check "status rolledback" '[ "$(field status)" = rolledback ]'
check "0.5.0 back in place" '[ "$("$T/data/soundya" version)" = "soundya 0.5.0" ]'
check "0.5.0 running again" '"$T/data/soundya" status >/dev/null'
check "config still there" 'grep -q 2-1-secret "$T/data/config.json"'
check "soundya.env kept" 'grep -q "X=1" "$T/data/soundya.env"'
check "failure log kept" '[ -f "$T/data/failed-update.log" ]'

echo "4. damaged package is refused before anything is touched"
package "$T/p4" 0.5.2 good
echo "garbage" >> "$T/p4/soundya"
sh "$T/p4/install.sh" >"$T/log4" 2>&1
check "status failed" '[ "$(field status)" = failed ]'
check "reason mentions damage" 'field reason | grep -q повреждён'
check "0.5.0 still installed and running" '[ "$("$T/data/soundya" version)" = "soundya 0.5.0" ] && "$T/data/soundya" status >/dev/null'

echo "5. config migrated by a bad version comes back as it was"
package "$T/p5" 0.5.3 crash
sh "$T/p5/install.sh" >"$T/log5" 2>&1 &
# The crashing version «migrates» the config while the installer waits.
sleep 1; echo '{"token":"","migrated":true}' > "$T/data/config.json"; wait
check "rolled back" '[ "$(field status)" = rolledback ]'
check "config as before the update" 'grep -q 2-1-secret "$T/data/config.json"'

echo "6. old snapshots are pruned to five"
for v in 0.5.4 0.5.5 0.5.6 0.5.7 0.5.8; do package "$T/p$v" "$v" good; sh "$T/p$v/install.sh" >/dev/null 2>&1; sleep 1; done
check "five data snapshots" '[ "$(ls -1d "$T"/data/backup/data-* | wc -l)" -eq 5 ]'

if [ "$FAILS" -ne 0 ]; then
    echo "$FAILS failed; logs:"; for f in "$T"/log*; do echo "== $f"; cat "$f"; done
    exit 1
fi
echo "all passed"
