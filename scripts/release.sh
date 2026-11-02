#!/bin/sh
# Публикует релиз: свежий код с GitHub, сборка, релиз на GitHub и в YAM.
#   sh scripts/release.sh            # версия из Cargo.toml
#   sh scripts/release.sh v0.4.37    # то же, но проверит, что версия та
set -eu
cd "$(dirname "$0")/.."
. scripts/fresh.sh
TAG="v$VERSION"
if [ -n "${1:-}" ] && [ "$1" != "$TAG" ]; then
    echo "в коде версия $VERSION, а просили $1: релиз не делаю" >&2
    exit 1
fi
# The version badge in README follows the release.
if ! grep -q "badge/версия-$VERSION-" README.md; then
    sed -i.bak "s#badge/версия-[0-9.]*-ff5500\" alt=\"Версия [0-9.]*\"#badge/версия-$VERSION-ff5500\" alt=\"Версия $VERSION\"#" README.md && rm -f README.md.bak
    git commit -q -m "README: версия $VERSION" README.md && git push -q
fi
NOTES=$(awk -v v="## $VERSION" '$0==v{f=1;next} /^## /{f=0} f' CHANGELOG.md)
if gh release view "$TAG" >/dev/null 2>&1; then
    echo "== $TAG уже есть на GitHub: заменяю файл и описание"
    gh release upload "$TAG" dist/soundya-armv7.tar --clobber
    gh release edit "$TAG" --notes "$NOTES"
else
    gh release create "$TAG" dist/soundya-armv7.tar --title "soundya $TAG" --notes "$NOTES"
fi
[ "${SKIP_YAM:-0}" = 1 ] || sh scripts/yam-release.sh
