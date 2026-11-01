#!/bin/sh
# Publishes soundya to the YAM Registry (https://repo.yse.wtf), one package per
# Station model:
#   export YAM_PUBLISH_TOKEN='…'                       # Profile → Publish token
#   export YAM_AUTHOR_KEY_FILE=~/Keys/yam-author-private-key.pem
#   sh scripts/yam-release.sh                          # after scripts/build-mac.sh
#
# Needs YAM's yam-publish (git clone https://github.com/nyakokitsu/yam ../yam &&
# cd ../yam && cargo build --release --locked --bin yam-publish), or set YAM_SRC.
# Users then install with:  yam install rodyatotu/soundya
set -eu
cd "$(dirname "$0")/.."
NS=${YAM_NS:-rodyatotu}
YAM_SRC=${YAM_SRC:-../yam}
PUBLISH="$YAM_SRC/target/release/yam-publish"
# Station Lite, Mini 1, Mini 2, Station 2: the models soundya runs on. Their
# userspace is 32-bit, so the same armv7 build goes into each.
TARGETS=${YAM_TARGETS:-"yandexmicro yandexmini yandexmini_2 yandexmidi"}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

: "${YAM_PUBLISH_TOKEN:?export YAM_PUBLISH_TOKEN (repo.yse.wtf → Профиль → Publish-токен)}"
: "${YAM_AUTHOR_KEY_FILE:?export YAM_AUTHOR_KEY_FILE (ключ, скачанный при создании namespace)}"
[ -x "$PUBLISH" ] || { echo "нет $PUBLISH: соберите yam-publish (см. начало скрипта)" >&2; exit 1; }
[ -x dist/soundya/soundya ] || { echo "нет dist/soundya: сначала sh scripts/build-mac.sh" >&2; exit 1; }

STAGE=dist/yam
rm -rf "$STAGE" && mkdir -p "$STAGE/bin" "$STAGE/share/$NS.soundya"
cp -R dist/soundya/. "$STAGE/share/$NS.soundya/"
cp packaging/yam/soundya-install "$STAGE/bin/soundya-install"
chmod 755 "$STAGE/bin/soundya-install"

for T in $TARGETS; do
    cat > "$STAGE/yam.yaml" <<YAML
name: $NS/soundya
version: $VERSION
license: MIT
description: SoundCloud на Станции голосом Алисы, AirPlay, управление ПК и ТВ, веб-панель
homepage: https://github.com/rodyatotu/soundya
targets: [$T]
files:
  bin/soundya-install: bin/soundya-install
  share/$NS.soundya/: share/$NS.soundya/
postinstall: [bin/soundya-install]
YAML
    echo "== $NS/soundya $VERSION для $T"
    "$PUBLISH" release \
        --source "$STAGE" \
        --profiles "$YAM_SRC/platforms" \
        --platform "$T" \
        --output "dist/soundya-$VERSION-$T.yampkg" \
        --token "$YAM_PUBLISH_TOKEN" \
        --key "$YAM_AUTHOR_KEY_FILE"
done
echo "Готово: yam install $NS/soundya"
