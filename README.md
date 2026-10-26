<p align="center">
  <img src="docs/cover.jpg" alt="soundya — дай жизнь своему трупу: SoundCloud × Яндекс Алиса" width="100%">
</p>

<h1 align="center">soundya</h1>

<p align="center">
  <b>SoundCloud на Яндекс Станции — голосом Алисы, без телефона и компьютера</b>
</p>

<p align="center">
  <a href="https://www.donationalerts.com/r/rodyatotu"><img src="https://img.shields.io/badge/Поддержать-DonationAlerts-ff5500?style=for-the-badge" alt="Поддержать через DonationAlerts"></a>
  <a href="#поддержать-проект"><img src="https://img.shields.io/badge/USDT-TRC--20-26a17b?style=for-the-badge&logo=tether&logoColor=white" alt="USDT TRC-20"></a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Станция_Лайт-проверено-2f9e5b" alt="Станция Лайт: проверено">
  <img src="https://img.shields.io/badge/Rust-armv7-dea584?logo=rust&logoColor=white" alt="Rust, armv7">
  <img src="https://img.shields.io/badge/лицензия-MIT-blue" alt="MIT">
  <img src="https://img.shields.io/badge/версия-0.2.0-ff5500" alt="Версия 0.2.0">
</p>

---

**soundya** — небольшая программа, которая живёт прямо на рутованной Яндекс Станции и играет музыку из SoundCloud. Говорите «Алиса, включи Bonobo» — и колонка сама находит трек и включает его. Управлять можно и с телефона: у колонки появляется своя веб-страница.

<p align="center">
  <img src="docs/showcase.png" alt="Веб-панель soundya: плеер, поиск, настройки" width="100%">
</p>

## Что умеет

<p align="center"><img src="docs/banner-recs.jpg" alt="Своя волна" width="100%"></p>


- 🎙 **Голос.** «Алиса, включи …», «пауза», «дальше», «перемешай», «включи мои лайки», «включи рекомендации», «что сейчас играет» — Алиса не уходит в Яндекс Музыку.
- 👤 **Ваш аккаунт SoundCloud.** Лайки, свои и сохранённые плейлисты, подборки SoundCloud.
- ✨ **Рекомендации.** «Волна» из треков, похожих на ваши лайки. Подбирается заранее, включается сразу.
- 📱 **Веб-панель** по адресу `http://soundya.local:8765`: плеер, поиск, очередь, настройки.
- 🔀 Перемешивание, повтор, «похожее», автопродолжение похожими треками в конце очереди.
- 🌐 **Прокси** (SOCKS5/HTTP) для мест, где SoundCloud недоступен, и **индикатор соединения** — видно, когда всё тормозит из-за сети.
- 🟠 **Оранжевая подсветка** кольца, пока играет SoundCloud.
- 💪 **Устойчивость к плохой сети:** трек скачивается заранее, при обрыве дозагружается и продолжается с того же места.

## Голосовые команды

<p align="center"><img src="docs/banner-voice.jpg" alt="Алиса, включи…" width="100%"></p>


| Сказать | Что произойдёт |
|---|---|
| «Алиса, включи **Bonobo**» / «включи песню …» | поиск и воспроизведение |
| «Алиса, включи **саундклауд**» | продолжить или включить ваши лайки |
| «включи **мои лайки**» / «включи **мой плейлист** осень» | ваша музыка (нужен вход) |
| «включи **рекомендации**» / «включи что-нибудь новое» | подборка по вашему вкусу |
| «включи плейлист …» | сначала среди ваших, потом в SoundCloud |
| «пауза», «стоп», «продолжи», «дальше», «назад» | управление |
| «**перемешай**» / «играй по порядку» | перемешивание |
| «поставь **на повтор**» / «выключи повтор» / «включи заново» | повтор |
| «включи **похожее**» | треки, похожие на текущий |
| «**что сейчас играет**?» | Алиса скажет название |

Команды управления (пауза, дальше и т. п.) soundya перехватывает только пока сам играет — в остальное время Алиса работает как обычно. В настройках можно включить режим «только по слову саундклауд»: тогда «включи Bonobo» остаётся Алисе, а «включи Bonobo **в саундклауде**» — soundya.

## Как это работает

<p align="center">
  <img src="docs/how-it-works.png" alt="Схема: Алиса распознаёт фразу, soundya ищет в SoundCloud и играет через динамик" width="100%">
</p>

soundya подключается к локальному интерфейсу Алисы на самой колонке (`127.0.0.1:9875`), получает распознанный текст фразы, отменяет ответ Алисы и сам ищет и проигрывает музыку. Звук идёт через штатный микшер колонки, поэтому Алиса может говорить поверх музыки — музыка в это время приглушается.

## Что нужно

- **Яндекс Станция Лайт** (1-го поколения). Проверено на прошивке `0.369.1.46`. Мини 1 по устройству похожа, но не проверялась.
- **Root-доступ по ADB** и **отключённые обновления** (см. шаг 1).
- Компьютер с `adb` — только для установки. Дальше колонка работает сама.
- Если SoundCloud у вас недоступен — **SOCKS5- или HTTP-прокси**.

## Установка

### 1. Root и отключение обновлений

Делается по гайдам сообщества — подробно и с картинками:

1. **Root по сети (DNS-метод):** [hackalice.me — шаг 1](https://hackalice.me/start/root/). Удобнее всего со своим стендом [lite-kit](https://github.com/krevetkqqq/lite-kit) на компьютере.
2. **Отключите обновления — обязательно**, иначе Яндекс пришлёт прошивку и root пропадёт: [hackalice.me — шаг 2](https://hackalice.me/start/ssh/). Коротко, из клона [yandex-station-extended](https://github.com/Marker284/yandex-station-extended):
   ```sh
   adb connect <IP-колонки>:5555
   adb shell 'mkdir -p /data/quasar && touch /data/quasar/activate_adb_quasmodrom /data/quasar/activate_network_adb && chattr +i /data/quasar/activate_adb_quasmodrom /data/quasar/activate_network_adb'
   adb shell mkdir -p /data/opt
   adb push contrib/yandex-noupdate.sh /data/opt/yandex-noupdate.sh
   adb shell sh /data/opt/yandex-noupdate.sh
   adb shell reboot
   ```
   Проверка после перезагрузки: `adb shell sh /data/opt/yandex-noupdate.sh status` → `updatesd убран`, `заглушён`, `подменён`.

### 2. Закрепите IP колонки в роутере

Чтобы колонка не меняла адрес после перезагрузки, закрепите его за ней в DHCP роутера:

- **TP-Link:** Advanced → Network → DHCP Server → Address Reservation → Add
- **Keenetic:** Список устройств → колонка → «Постоянный IP-адрес»
- **ASUS:** ЛВС → DHCP-сервер → «Список назначенных вручную IP-адресов»

Веб-панель и так доступна по имени **`soundya.local`** (mDNS), но закреплённый IP пригодится для `adb` и для устройств, которые не понимают `.local`.

### 3. Получите soundya

**Готовая сборка:** скачайте `soundya-armv7.tar` со страницы [Releases](../../releases).

**Или соберите сами** (macOS или Linux):
```sh
curl https://sh.rustup.rs -sSf | sh -s -- -y && source ~/.cargo/env
rustup target add armv7-unknown-linux-gnueabihf
brew install zig cargo-zigbuild          # на Linux: pip install ziglang && cargo install cargo-zigbuild
git clone https://github.com/rodyatotu/soundya && cd soundya
sh scripts/build-mac.sh                  # → dist/soundya-armv7.tar
```

### 4. Установите на колонку

Из папки проекта:
```sh
sh scripts/deploy.sh <IP-колонки>
```

Или вручную, если скачали готовую сборку:
```sh
adb connect <IP-колонки>:5555
adb push soundya-armv7.tar /tmp/
adb shell "cd /tmp && tar -xf soundya-armv7.tar && sh /tmp/soundya/install.sh"
```

Установщик кладёт всё в `/data/opt/soundya`, добавляет автозапуск и команду `soundya`. Настройки при обновлении сохраняются.

### 5. Настройте

<p align="center"><img src="docs/banner-web.jpg" alt="Пульт в телефоне" width="100%"></p>


Откройте **http://soundya.local:8765** (или `http://<IP-колонки>:8765`) → **Настройки**:

1. **Прокси** — если SoundCloud у вас открывается только через него. Кнопка «Проверить прокси» покажет, работает ли он.
2. **Аккаунт SoundCloud** — для лайков, плейлистов и рекомендаций. SoundCloud не даёт сторонним приложениям вход по паролю, поэтому нужен токен из браузера: войдите на soundcloud.com → инструменты разработчика (Cmd+Option+I / F12) → Application → Cookies → `https://soundcloud.com` → значение `oauth_token`. Токен — как пароль: никому его не отправляйте.

Всё. Скажите: **«Алиса, включи рекомендации»**.

## Настройки на колонке

Файл `/data/opt/soundya/soundya.env` (после правки — `/etc/init.d/S99soundya restart`):

| Переменная | По умолчанию | Что делает |
|---|---|---|
| `SOUNDYA_WEB_PORT` | `8765` | порт веб-панели, `0` — выключить |
| `SOUNDYA_NAME` | `soundya` | имя в сети: `http://<имя>.local:8765` |
| `SOUNDYA_DEVICE` | `default` | ALSA-устройство вывода |
| `SOUNDYA_DNS` | — | свои DNS-серверы, если системный DNS колонки не работает |

Прокси, вход, «только по слову саундклауд» и подсветка настраиваются на странице и хранятся в `/data/opt/soundya/config.json`.

## Команды на колонке

```sh
soundya play <запрос | ссылка>   soundya playlist <запрос>   soundya likes   soundya recs
soundya pause | resume | toggle | next | prev | stop
soundya shuffle on|off           soundya repeat on|off       soundya similar
soundya status | queue
/etc/init.d/S99soundya start | stop | restart
```
Лог: `/tmp/soundya.log`.

## Если что-то не так

<details>
<summary><b>Алиса отвечает «нужна подписка на Яндекс Музыку»</b></summary>

Значит, фразу забрала Алиса. Посмотрите в логе строки `voice: …`: там видно, что soundya услышал и почему отдал Алисе. Команды управления («дальше», «пауза») перехватываются, только пока играет SoundCloud.
</details>

<details>
<summary><b>Долго грузится, Алиса долго «думает»</b></summary>

Откройте «Настройки» → «Соединение с SoundCloud». Если пинг больше ~500 мс или скорость меньше ~600 кбит/с — тормозит прокси или сеть. Попробуйте другой прокси.
</details>

<details>
<summary><b>Трек «играет», но звука нет (время стоит на 0:00)</b></summary>

`adb shell 'grep -E "^(player|alsa)" /tmp/soundya.log | tail'`. Если звуковая карта не запускается — перезагрузите колонку (`adb shell reboot`): это сбрасывает общий звуковой микшер.
</details>

<details>
<summary><b>Страница по адресу soundya.local не открывается</b></summary>

Откройте по IP колонки (`http://<IP>:8765`). Некоторые старые Android-устройства не понимают адреса `.local`.
</details>

<details>
<summary><b>Почему нельзя ставить лайки и добавлять в плейлисты из soundya</b></summary>

SoundCloud отвечает на такие запросы капчей своей антибот-защиты — изменения он разрешает только из браузера или приложения. Кнопка «↗» в плеере открывает текущий трек на SoundCloud, где лайк ставится в одно нажатие. Читать лайки и плейлисты это не мешает.
</details>

## Ограничения

- Треки Go+ без подписки и часть лицензированных треков SoundCloud отдаёт только 30-секундным превью — такие пропускаются.
- Используется неофициальный API сайта SoundCloud: если они его поменяют, понадобится обновление.
- Проверено только на Станции Лайт 1-го поколения.

## Благодарности

- [**spotiya**](https://github.com/klockky/spotiya) (MIT) — протокол локального интерфейса Алисы, на котором держится голосовое управление.
- [**yandex-station-extended**](https://github.com/Marker284/yandex-station-extended) и [**hackalice.me**](https://hackalice.me) — гайды по руту, скрипт отключения обновлений, описание озвучки через Алису.
- [**lite-kit**](https://github.com/krevetkqqq/lite-kit) — стенд для получения root.
- [symphonia](https://github.com/pdeljanov/Symphonia), [ureq](https://github.com/algesten/ureq), [serde](https://serde.rs).

## Важно

soundya — неофициальный проект и никак не связан с Яндексом и SoundCloud. Root-доступ и изменение прошивки — на ваш страх и риск: гарантию на колонку это может лишить. Пользуйтесь SoundCloud в рамках его правил.

## Поддержать проект

Если soundya вам пригодился — можно сказать спасибо:

<p align="center">
  <a href="https://www.donationalerts.com/r/rodyatotu"><img src="https://img.shields.io/badge/Задонатить-DonationAlerts-ff5500?style=for-the-badge" alt="DonationAlerts"></a>
</p>

<p align="center">
  <img src="docs/usdt-qr.png" alt="USDT TRC-20: TSEJPa8FefWd8u8mo4USu8muSJr7589gWg" width="280">
</p>

<p align="center"><b>USDT · TRC-20 (Tron)</b> — только эта сеть<br><code>TSEJPa8FefWd8u8mo4USu8muSJr7589gWg</code></p>

## Лицензия

[MIT](LICENSE)

<p align="center"><img src="docs/logo.jpg" alt="soundya" width="120"></p>
