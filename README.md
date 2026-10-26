# soundya — SoundCloud на Яндекс Станции Мини 1

Колонка сама ищет и играет музыку из SoundCloud по голосу. Телефон и ПК не нужны.

- «Алиса, включи Bonobo», «включи песню Kiara», «включи плейлист lofi beats»
- «пауза», «продолжи», «следующий трек», «назад»
- когда очередь кончается, играют похожие треки (как автоплей в SoundCloud)
- пока Алиса слушает или отвечает, музыка приглушается
- громкость общая с Алисой («громче», «тише», кнопки)
- веб-страница в домашней сети: управление, очередь и настройка прокси

Нужен root по сети (ADB) и отключённые обновления.

## Как устроено

Один бинарник на Rust, без зависимостей на колонке:

- голос: локальный IPC Алисы `127.0.0.1:9875` (протокол взят из [spotiya](https://github.com/klockky/spotiya), MIT). Распознанная фраза «включи …» перехватывается, ответ Алисы отменяется, её плеер ставится на паузу;
- SoundCloud: неофициальный `api-v2` (как у сайта и yt-dlp), `client_id` берётся со страницы soundcloud.com и обновляется сам;
- звук: MP3/AAC декодирует symphonia, вывод в ALSA `default` через системную `libasound`.

## Сборка

### Через GitHub Actions (проще всего, работает с Mac)

1. Создайте на GitHub пустой репозиторий и загрузите туда содержимое этой папки.
2. Вкладка **Actions** → workflow **Build soundya** запустится сам.
3. Через ~10 минут скачайте артефакт `soundya-armv7` (внутри `soundya-armv7.tar.gz`).

### Локально (Linux x86_64, WSL или Docker)

```sh
sh scripts/build-arm.sh          # нужен rustup и curl
```

Mac с Apple Silicon через Docker:

```sh
docker run --rm --platform linux/amd64 -v "$PWD":/src -w /src rust:1 sh scripts/build-arm.sh
```

Результат: `dist/soundya-armv7.tar.gz`.

## Установка

```sh
adb connect <IP-колонки>:5555
adb push soundya-armv7.tar.gz /tmp/
adb shell "cd /tmp && tar -xzf soundya-armv7.tar.gz && sh /tmp/soundya/install.sh"
```

Установщик кладёт всё в `/data/opt/soundya`, добавляет автозапуск `/etc/init.d/S99soundya` и команду `soundya`.

## Проверка по шагам

```sh
adb shell
soundya search bonobo        # 1. SoundCloud отвечает? должен быть список треков
soundya once bonobo          # 2. звук идёт? (Ctrl+C чтобы остановить)
soundya play bonobo          # 3. через демон
soundya status
```

Потом голосом: «Алиса, включи Bonobo».

Лог: `/tmp/soundya.log`. В нём видно `voice: connected to Alice`, распознанные фразы и ошибки.

## Команды

```
soundya play <запрос | ссылка soundcloud.com>
soundya playlist <запрос>
soundya likes
soundya pause | resume | toggle | next | prev | stop
soundya status | queue
/etc/init.d/S99soundya start | stop | restart
```

## Веб-страница

Откройте в браузере из домашней сети `http://<IP колонки>:8765/`. Там:

- что играет, прогресс, кнопки ⏮ ⏯ ⏭ ⏹;
- **Поиск**: список треков и плейлистов, нажатие включает выбранное (треки ниже по списку идут следом). Можно вставить ссылку soundcloud.com;
- **Моя музыка**: вход в аккаунт SoundCloud, лайки и ваши плейлисты (свои, сохранённые и миксы SoundCloud). Голосом: «Алиса, включи мои лайки», «Алиса, включи мой плейлист …»;
- **Очередь**: нажатие на трек переключает на него;
- **Настройки**:
- **прокси**: `http://host:port`, `socks5://host:port` или `socks5://user:pass@host:port`. Кнопка «Проверить» показывает, открываются ли через него soundcloud.com и API; «Сохранить» применяет сразу, без перезапуска. Через прокси идут и поиск, и звук;
- галочка «только по слову саундклауд».

Настройки страницы хранятся в `/data/opt/soundya/config.json`. Пароль прокси и токен на странице скрыты точками.

### Вход в SoundCloud

Пароль SoundCloud сторонним приложениям не принимает (там капча), поэтому вход по токену из браузера: войдите на soundcloud.com, откройте инструменты разработчика (Cmd+Option+I), вкладка Application → Cookies → `https://soundcloud.com`, скопируйте значение `oauth_token` и вставьте на странице во вкладке «Моя музыка». Токен живёт долго; если SoundCloud его отзовёт (например, после выхода из аккаунта в браузере), страница скажет об этом, и нужно будет вставить новый.

С подпиской Go+ через вход доступны полные версии треков, которые без неё играют только как превью.

Страница без пароля: её видит любой в вашей Wi-Fi-сети.

Прочее в `/data/opt/soundya/soundya.env` (после правки `/etc/init.d/S99soundya restart`):

- `SOUNDYA_WEB_PORT`: порт страницы, `0` отключает её;
- `SOUNDYA_DEVICE`: ALSA-устройство (по умолчанию `default`).

## Удаление

```sh
/etc/init.d/S99soundya stop
mount -o remount,rw /
rm /etc/init.d/S99soundya /usr/bin/soundya
mount -o remount,ro /
rm -r /data/opt/soundya
```

## Ограничения

- Треки Go+ и часть лицензированных отдаются как 30-секундные превью или с DRM: они пропускаются.
- Opus-потоки не поддерживаются, берутся MP3 или AAC (они есть почти у всех треков).
- API неофициальный: если SoundCloud его поменяет, нужно будет обновить код.
- «Включи музыку» без названия продолжает SoundCloud, только если очередь уже есть; иначе остаётся Алисе.
