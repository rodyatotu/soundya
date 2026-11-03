#!/bin/sh
# Publishes soundya to the YAM Registry (https://repo.yse.wtf) as a Luna package
# (github.com/nyakokitsu/luna): Luna starts it, shows it among its media
# providers and lends it the ring. One artifact per Station model:
#   export YAM_PUBLISH_TOKEN='…'                       # Профиль → Publish-токен
#   export YAM_AUTHOR_KEY_FILE=~/Keys/yam-author-private-key.pem
#   sh scripts/yam-release.sh        (сам делает git pull и сборку; release.sh вызывает его)
#
# Needs YAM's yam-publish (git clone https://github.com/nyakokitsu/yam ../yam &&
# cd ../yam && cargo build --release --locked --bin yam-publish), or set YAM_SRC.
# Only build the .yampkg files to try on a speaker, without publishing:
#   YAM_PACK_ONLY=1 sh scripts/yam-release.sh
#   adb push dist/soundya-<версия>-yandexmicro.yampkg /data/local/tmp/
#   adb shell yam install --yes /data/local/tmp/soundya-<версия>-yandexmicro.yampkg
# Users then install with:  yam install rodyatotu/soundya
# and enable it in Luna (binary plugins on, the package switched on).
set -eu
cd "$(dirname "$0")/.."
. scripts/fresh.sh
NS=${YAM_NS:-rodyatotu}
ID="$NS.soundya"
# Defaults, so a release is just «sh scripts/yam-release.sh»:
#   ~/yam                 YAM sources with yam-publish built
#   ~/Keys/yam-key.pem    the namespace key
#   ~/Keys/yam-token      the publish token (one line, chmod 600)
if [ -z "${YAM_SRC:-}" ]; then
    if [ -d ../yam ]; then YAM_SRC=../yam; else YAM_SRC="$HOME/yam"; fi
fi
[ -n "${YAM_AUTHOR_KEY_FILE:-}" ] || [ ! -f "$HOME/Keys/yam-key.pem" ] || YAM_AUTHOR_KEY_FILE="$HOME/Keys/yam-key.pem"
[ -n "${YAM_PUBLISH_TOKEN:-}" ] || [ ! -f "$HOME/Keys/yam-token" ] || YAM_PUBLISH_TOKEN=$(tr -d ' \r\n' < "$HOME/Keys/yam-token")
PUBLISH="$YAM_SRC/target/release/yam-publish"
# Station Lite, Mini 1, Mini 2, Station 2: the models soundya runs on.
TARGETS=${YAM_TARGETS:-"yandexmicro yandexmini yandexmini_2 yandexmidi"}

PACK_ONLY=${YAM_PACK_ONLY:-0}
if [ "$PACK_ONLY" != 1 ]; then
    : "${YAM_PUBLISH_TOKEN:?export YAM_PUBLISH_TOKEN (repo.yse.wtf → Профиль → Publish-токен)}"
    : "${YAM_AUTHOR_KEY_FILE:?export YAM_AUTHOR_KEY_FILE (ключ, скачанный при создании namespace)}"
fi
[ -x "$PUBLISH" ] || { echo "нет $PUBLISH: соберите yam-publish (см. начало скрипта)" >&2; exit 1; }
[ -x dist/soundya/soundya ] || { echo "нет dist/soundya: сначала sh scripts/build-mac.sh" >&2; exit 1; }

STAGE=dist/yam
PLUGIN="$STAGE/plugin"
rm -rf "$STAGE" && mkdir -p "$PLUGIN"
cp dist/soundya/soundya dist/soundya/shairport-sync dist/soundya/nfqws dist/soundya/nfqws2 "$PLUGIN/"
cp -R dist/soundya/fake "$PLUGIN/fake"
cp -R dist/soundya/lua "$PLUGIN/lua"
cp packaging/luna/soundya-luna "$PLUGIN/soundya-luna"
chmod 755 "$PLUGIN/soundya" "$PLUGIN/shairport-sync" "$PLUGIN/nfqws" "$PLUGIN/nfqws2" "$PLUGIN/soundya-luna"
cat > "$PLUGIN/plugin.toml" <<TOML
id = "$ID"
name = "soundya — SoundCloud"
version = "$VERSION"
entry = "soundya-luna"
description = "SoundCloud голосом Алисы, AirPlay, ПК и ТВ, веб-панель на :8765"
permissions = ["exec", "media.provider", "led"]

[launch]
kind = "binary"
mode = "service"
interpreter = "sh"

[media_provider]
label = "SoundCloud"
actions = ["pause", "resume", "next", "previous"]
TOML

for T in $TARGETS; do
    cat > "$STAGE/yam.yaml" <<YAML
name: $NS/soundya
version: $VERSION
license: MIT
description: SoundCloud на Станции голосом Алисы, AirPlay, управление ПК и ТВ, веб-панель (пакет для Luna)
homepage: https://github.com/rodyatotu/soundya
targets: [$T]
depends: [yam/luna]
files:
  share/luna/plugins/$ID/: plugin/
YAML
    echo "== $NS/soundya $VERSION для $T"
    if [ "$PACK_ONLY" = 1 ]; then
        "$PUBLISH" pack --source "$STAGE" --profiles "$YAM_SRC/platforms" --platform "$T" \
            --output "dist/soundya-$VERSION-$T.yampkg"
    else
        # A version already in YAM cannot change: skip it, publish the rest.
        if ! OUT=$("$PUBLISH" release \
            --source "$STAGE" \
            --profiles "$YAM_SRC/platforms" \
            --platform "$T" \
            --output "dist/soundya-$VERSION-$T.yampkg" \
            --token "$YAM_PUBLISH_TOKEN" \
            --key "$YAM_AUTHOR_KEY_FILE" 2>&1); then
            case "$OUT" in
                *immutable*) echo "   $VERSION для $T уже есть в YAM, пропускаю (для новой сборки поднимите версию)" ;;
                *) echo "$OUT" >&2; exit 1 ;;
            esac
        else
            echo "$OUT"
        fi
    fi
done
echo "Готово: yam install $NS/soundya, затем включите пакет в Luna"
