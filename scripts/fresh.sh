# Sourced by release.sh, yam-release.sh and deploy.sh: takes the latest code
# from GitHub and builds it, so nothing goes out from an old checkout.
# The calling script starts again once if git pull changed it.
if [ -z "${SOUNDYA_FRESH:-}" ]; then
    echo "== git pull"
    git pull --ff-only || { echo "git pull не прошёл: разберитесь с локальными правками (git status)" >&2; exit 1; }
    SOUNDYA_FRESH=1 exec sh "$0" "$@"
fi
if [ -z "${SOUNDYA_BUILT:-}" ]; then
    sh scripts/build-mac.sh
    export SOUNDYA_BUILT=1
fi
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
