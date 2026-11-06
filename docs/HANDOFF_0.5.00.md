# soundya 0.5.00 — передача работы (состояние на 2026-11-06)

Документ для продолжения работы в другом чате: что такое проект, как он устроен,
что сделано в ветке `release-0.5.00`, чем проверено, что не проверено и что
осталось. Источники подробностей: `RELEASE_0.5.00_AUDIT.md` (аудит, баги Б-1…Б-21),
`RELEASE_0.5.00_PLAN.md` (план P0–P3 и статусы), `RELEASE_0.5.00_TESTS.md`
(автотесты и чек-листы на колонках), `RELEASE_0.5.00_CHANGELOG.md` (изменения
человеческим языком), `docs/voice-priority.md` (кому достаётся фраза).

---

## 0. Правила владельца (из брифа, действуют всегда)

- Не переписывать работающее, не плодить дублирующие системы, без мёртвых кнопок и заглушек.
- Не заявлять поддержку железа, которое не проверено. Не удалять настройки, старые конфиги должны работать.
- **Секреты не хранить в исходниках.** Не публиковать секреты и пользовательские данные.
- Каждая новая функция: бэкенд + фронтенд + ошибки + логи + настройки + статус + выключатель + документация + кнопка «Сообщить об ошибке».
- Багрепорты без согласия не отправляют пароли, токены, cookie, OAuth, ключи, переписку, аудио; перед отправкой видно всё, что уйдёт.
- **Не публиковать релиз без отдельного разрешения.** Последнее указание: «пока что не релизь на гитхаб».
- **Не делать вид, что тестирование прошло, если его не было.**
- Владелец тестирует локально и коммитит/пушит сам. Общение — по-русски.

## 1. Состояние репозитория

- Репозиторий: `github.com/rodyatotu/soundya`, ветка **`release-0.5.00`**, 48 коммитов поверх
  `060760f` (это `origin/release-0.4.41` с работой автора, влитой в ветку).
- **Ничего не запушено**: у облачной сессии не было прав (403, GitHub не привязан), а
  последний пуш отклонил фильтр — владелец просил не публиковать. Код передаётся
  bundle-файлом (см. §11).
- Версия в `Cargo.toml` всё ещё **0.4.41** (в т.ч. файл `VERSION` в архиве колонки). По плану
  перед релизом → `0.5.0` (semver; на страницах — «0.5.00»). `installer/Cargo.toml` = 0.5.0.
- 124 юнит-теста зелёные; все E2E-скрипты (§8) проходят.

## 2. Что это за проект

soundya — программа на Rust для **рутованных Яндекс Станций** (Лайт, Мини 1/2/3, Станция 2;
Luna-пакет для Plum/Pickle). Колонка играет **SoundCloud по голосу Алисы**: soundya слушает
распознанный текст Алисы через локальный Quasar IPC (`127.0.0.1:9875`), перехватывает свои
фразы (отменяет ответ Алисы) и играет звук через ALSA. Плюс: AirPlay-приёмник
(shairport-sync), веб-панель на `:8765`, управление ПК (агент `pc/` на PowerShell),
Android TV/проектор по ADB, обход блокировок (zapret/nfqws), эквалайзер, подсветка кольца,
экран-часы, планировщик + Google, Telegram-боты, багрепорты на сервер автора.

## 3. Архитектура программы на колонке (`src/`)

**Модель исполнения:** Rust 2021, без async. Один поток **Controller** (`main.rs`) владеет
состоянием плеера и настройками и получает команды через `mpsc::Receiver<Cmd>`. Остальное —
отдельные потоки: веб-сервер (поток на соединение, максимум 48), слушатель Алисы,
плеер, свет, дисплей, календарь, планировщик, Telegram, AirPlay-супервизор и т.д.
Общий снимок для потоков — `Shared` (мьютексы/атомики: прокси, играет ли, макросы,
контакты…). Сетевые вызовы — `ureq` (через прокси из настроек: `sc::build_agent`).
Декодирование — `symphonia` (mp3/aac/mp4). Без tokio, бинарник маленький (opt-level z, LTO).

**`Cmd`** (основные): `Control(line, reply)` — текстовые команды из панели/CLI/макросов;
`Voice(Intent)` — разобранная фраза; `Settings(json patch, reply)` — слияние настроек;
`Loaded`/`Results`/`Net`… — ответы рабочих потоков.

**Статус** — JSON, который панель опрашивает раз в секунду (`/api/status`, кэш 1 с):
трек, очередь, настройки (секреты замаскированы `••••`), led, device, ducking, voice,
telegram, display, calendar, listen, onboarding, login (SoundCloud) и т.д.

### Модули (строк кода)

| Модуль | Назначение |
|---|---|
| `main.rs` (2989) | Controller, `Cmd`, все текстовые команды (`play/pause/next/…`, `tv …`, `pc …`, `macro …`, `plan …`, `google …`, `tg …`, `display …`, `probe`, `update`, `hello …`, `pin`, `calendar`…), статус, применение настроек (`apply_settings`, маскированные секреты = «без изменений»), автозапуск, продолжение музыки после перезагрузки, CLI (`soundya daemon|version|play|status|…`) |
| `alice.rs` (1786) | Quasar IPC, разбор фраз → `Intent`, `owns()` — чья фраза (soundya/Алиса), `normalize_phrase`, макросы (только фраза целиком), отмена ответа Алисы, `say()` |
| `sc.rs` (1401) | SoundCloud api-v2 (неофициальный), client_id, поиск, лайки, плейлисты, HLS, токен (проверка `/me`, одна 401 больше не сбрасывает вход; перепроверка раз в 10 мин) |
| `planner.rs` (1572) | Задачи/напоминания, русский разбор дат («через 10 минут», «12 марта», «до пятницы», «завтра вечером»), сводки (`Ask`: Plans/Todo/Schedule/Next/Deadlines/Week/Brief), предупреждения о сроке, утренняя сводка, голосовые команды, запись в Google |
| `calendar.rs` (707) | Источники событий: iCal по секретной ссылке (RRULE/EXDATE/RECURRENCE-ID, TZID = пояс колонки) + кэш Google (события/задачи, файл `google-cache.json`), напоминания о событиях, запись в Google (`add_event/add_task/complete_task/delete_task`) |
| `google.rs` (562) | OAuth (свой клиент пользователя, PKCE S256, код вставляется из адресной строки), refresh, Calendar API (calendarList/events), Tasks API (lists/tasks/insert/patch/delete), RFC 3339 |
| `web.rs` (467) + `web.html` (3543) | HTTP-сервер панели: CSRF/Host-проверка (`allowed()`), PIN (`auth.rs`), лимит соединений, `/api/stream` (звук в браузер) |
| `settings.rs` (842) | `config.json`: fsync + `.bak`, восстановление из `.bak`, `config_version` + миграция, `extra` (неизвестные поля сохраняются), merge-патчи |
| `report.rs` (847) | Багрепорты: модули, вложения (system.log, error.log, diagnostics.json, скриншот), вырезание секретов, очередь на флеше с повторами, формат сообщения |
| `led.rs` (904) | Кольцо: режимы solid/pulse/eq/wave/rainbow/breathe/running, порядок каналов, max_brightness, анимации установки по профилю (3 зоны / 6 зон), Mini 2 red/blue swap |
| `display.rs` (448) | Экран-часы (Mini 3/Plum/Pickle; разметка сегментов из Luna, MIT): виджеты время/ПК/погода (Open-Meteo), уведомления |
| `airplay.rs` (769) | shairport-sync как приёмник, ресемплер 44.1→48 кГц |
| `update.rs` (394) | Проверка релизов GitHub, скачивание, sha256, запуск `install.sh` в фоне, результат обновления |
| `telegram.rs` (378) | Свои боты пользователя: чтение сообщений вслух, разрешённые/неизвестные, тихие часы, «ответь …» с подтверждением |
| `pc.rs` (312) + `sha256.rs` | Команды на ПК, подпись HMAC-SHA256 (время+nonce+путь+тело), legacy-фолбэк; метрики, процессы, WoL |
| `tv.rs` (333) + `adb.rs` (690) | ADB-клиент (свой, без adb-бинарника: CNXN/AUTH RSA, shell, **push** через sync), Android TV: приложения, кнопки, поиск |
| `ducking.rs` (241) | Приглушение при звонках/уведомлениях (`/api/duck`, агент ПК видит занятый микрофон) |
| `voice.rs` (155) | Громкость голоса Алисы (временно понижает микшер, пока она слушает/говорит) |
| `hello.rs` (192) | Голосовой онбординг (адрес панели, «повтори адрес», шаги мастера, «спасибо») |
| `probe.rs` (271) | «Проверить возможности колонки» (Bluetooth/AirPlay 2/мультирум; только чтение) |
| `auth.rs`, `history.rs`, `mdns.rs` (`soundya.local`), `dns.rs`, `clock.rs`, `bypass.rs`/`zapret.rs`/`ipt.rs` (обход блокировок), `alsa.rs` (libasound через dlopen), `volume.rs`, `buttons.rs`, `vis.rs`, `buffer.rs`, `player.rs`, `luna.rs`, `stats.rs`, `fsx.rs`, `eq.rs` | см. первую строку `//!` каждого файла |

### Файлы на колонке

`/data/opt/soundya/` (state dir, `SOUNDYA_DIR` для тестов): `soundya`, `run.sh`, `guard.sh`,
`config.json(.bak)`, `planner.json`, `history.json`, `client_id`, `adbkey`, `install_id`,
`calendar.ics`, `google-cache.json`, `resume.json`, `brief-day`, `reports/` (очередь),
`backup/prev` + `backup/data-*` (5 последних), `update-result`, `previous.log`.
`/run/soundya.ready` («версия pid» — проверка здоровья), `/tmp/soundya.log`.
Init: `/etc/init.d/S99soundya` (`nohup setsid run.sh`).

### HTTP API панели (`web.rs`)

`GET /api/status`, `GET /api/community`, `GET /api/report/list`; `POST /api/cmd` (текстовая
команда Controller'а), `/api/settings` (merge-патч), `/api/duck`, `/api/listen`,
`/api/stream?id&t`, `/api/report`, `/api/report/preview`, `/api/report/retry`,
`/api/feedback` (старый), `/api/pin`, `/api/login`, `/api/test`. Все изменяющие
запросы — только с Origin/Host самой панели; при PIN — только авторизованные.

### Голос: кто владеет фразой

Порядок правил — `docs/voice-priority.md` (21 правило) + тест `alice::tests::priority_table`.
Кратко: свой сценарий (фраза целиком) → контакт ПК → ТВ → адрес → громкость голоса →
Telegram → планировщик → приглушение → перемотка → ПК → «… в яндекс музыке» →
собственное Я.Музыки (подкасты/плейлист дня) → SoundCloud-музыка → умный дом всегда Алисе.
Режим «Музыка по умолчанию: Яндекс Музыка» отдаёт музыку Алисе, кроме «… в саундклауде».

### Обновление / установка (`packaging/install.sh`)

1) проверка пакета (`SHA256SUMS`, новый бинарник запускается) → 2) копия программы
(`backup/prev`) и данных (`backup/data-*`) → 3) замена только файлов программы (данные не
трогаются) → 4) старт и проверка здоровья (`/run/soundya.ready` = нужная версия, живёт 10 с,
отвечает на `soundya status`) → иначе откат программы и данных. Итог в `update-result`.
Тестовые переопределения: `SOUNDYA_INSTALL_DIR/INIT/RUN/HEALTH_WAIT/HEALTH_HOLD`.

## 4. Настольные установщики и приложение для Mac (`installer/`)

- Отдельный крейт (свой `[workspace]`), зависимость только `serde_json`. Включает
  `dist/soundya-armv7.tar` (`include_bytes!`; тест-переопределение `SOUNDYA_INSTALLER_TAR`).
  Переиспользует `src/adb.rs` и `src/fsx.rs` через `#[path]`.
- Локальный сервер на `127.0.0.1:<случайный порт>` + мастер `wizard.html`
  (шаги hello → find → features → install → music → pc → done). API: `/api/info|ping|scan|probe|install|job|settings|pc|open|quit`.
- Поиск колонки: `soundya.local`, затем перебор `/24` на порту 5555; ключ ADB создаётся сам.
- Установка: `adb push` архива (с повторами) → проверка размера → распаковка и запуск
  `install.sh` **в фоне с защитой от SIGHUP** (`trap '' HUP` до `nohup setsid`, ожидание
  появления журнала; иначе запуск в обычном режиме) → прогресс по строкам журнала
  `install.sh` (`progress_of`) → результат из `update-result`.
  *История:* в бете окно «зависало на 95 %» — `install.sh` погибал от SIGHUP при закрытии
  сессии ADB (наш ADB использует старый `shell:` через PTY). Воспроизведено в имитации
  (старая команда 0/20 выживших, новая 20/20).
- **Mac:** `soundya.app` — тот же бинарник; на macOS открывает **своё окно**
  (`installer/src/macwin.rs`: NSApplication + меню «Правка» + NSWindow + WKWebView с
  `?app=1`, всё через Objective-C runtime и `dlopen`, без SDK). Если окно не создалось —
  фолбэк в браузер. `Info.plist`: `NSLocalNetworkUsageDescription` (macOS 15), ATS для
  локальной сети, иконка `soundya.icns`.
- **DMG на Linux** (`scripts/mac-dmg/`): `make_assets.py` (фон 660×420 pt @2x в стиле
  приложения: «Перетащите soundya в «Программы»», «Разреши все необходимые доступы <3», и
  .icns из `docs/logo.png`), `build_dmg.py` (xorriso `-hfsplus` гибрид, симлинк
  `/Applications`, `.DS_Store` с размером окна/позициями значков/alias на фон — CNID
  читаются из образа и образ собирается второй раз), `hfsinfo.py` (читатель каталога HFS+
  для проверки), `sign-on-mac.sh` (на Mac: ad-hoc/Developer ID подпись + пересборка через hdiutil).
- **Windows:** `soundya-setup-windows.exe` — тот же мастер, открывается в браузере;
  умеет настроить сам ПК (soundya-pc, `install.ps1 -Quiet`).
- Старые пути тоже есть: `scripts/mac-installer/Установить soundya.command`,
  `install-windows.cmd/ps1`, `scripts/deploy.sh` (везде та же защита от SIGHUP).

## 5. Сервер автора (`server/`) и агент ПК (`pc/`)

- `server/stats.py`: статистика установок, `/report` (багрепорт → Telegram
  `sendMessage` + `sendDocument`, дедупликация по ID, лимит частоты по
  `X-Forwarded-For` за Caddy, успешные только), `--selftest`. Нужен новый деплой на сервер.
- `server/community-bot.py`: заявки в сообщество — правила в личку, приём только после
  «Соглашаюсь», «Не соглашаюсь» → отказ, `/stats`, `/ban`, `/unban`.
- `pc/soundya-pc.ps1`: HTTP-агент на ПК: проверка HMAC (время, nonce, анти-повтор,
  `allow_plain_key` для старых колонок), метрики (CPU/GPU/RAM/темп.), процессы,
  приглушение по занятому микрофону. `pc/install.ps1 -Quiet` пишет `setup.json`.

## 6. Что сделано в 0.5.00 (по разделам)

**P0 надёжность:** надёжные настройки + миграция; безопасное обновление с откатом;
багрепорты (очередь, вырезание секретов, формат, Telegram); EQ без «тишины» (лимитер);
LED max_brightness и порядок каналов, цвета Mini 2; маршрутизация музыки
(«Музыка по умолчанию»); CSRF-защита панели; лимит тела 413; лимит соединений; PIN.

**P1:** статус входа SoundCloud и автоперепроверка токена, вход закладкой;
баннер «100+»; голосовой онбординг + мастер первой настройки; анимации установки по
моделям (кольцо 3/6 зон, экран-часы); Smart Ducking; громкость голоса Алисы;
«Где играет: колонка/здесь» (звук в браузере, колонка затихает); новый вид панели
(Liquid Glass, настраиваемый фон, вкладки Главная/Поиск/Моя музыка/Очередь/Планы/Настройки/Помощь);
таблица приоритетов голоса.

**P2:** редактор сценариев (ТВ/ПК/музыка, приложения ТВ); ПК — HMAC, метрики, процессы;
Telegram-боты; бот сообщества; **планировщик полноценно** (сводки, сроки, утро, запись в
Google Календарь и Google Задачи, вкладка «Планы»); календарь по iCal; экран-часы с виджетами.

**P3:** Ambilight-эффекты; автозапуск сценариев и продолжение музыки после перезагрузки (<20 мин);
«Проверить возможности колонки» (первый шаг к Bluetooth/AirPlay 2/мультируму).

**Установщики:** графические для Windows и Mac, Mac-приложение в DMG «перетащите в
Программы» со своим окном; исправлено «зависание на 95 %».

## 7. Конфигурация (основные поля `config.json`, `settings.rs`)

Музыка: `token`, `music_default` ("soundcloud"/"yandex"), `keyword_only` (синхронизируется),
`proxy`/`proxy_on`, `bypass*`. Свет: `light_*`, `led_order`, `ring`. Звук: `eq`, `duck_*`,
`alice_voice`, `alice_voice_max`. Устройства: `pc_*` (`pc_token` секрет), `pc_contacts`,
`tv_*`, `macros`, `autostart`, `resume_music`. Telegram: `tg_on`, `tg_bots` (токены — секреты),
`tg_allowed`, `tg_read`, `tg_max`, `tg_quiet_*`. Дисплей: `display_*`. Планы: `planner_on`,
`tz_offset` (минуты), `plan_warn` (30), `plan_due_time` (1080 = 18:00), `brief_on/at/days`,
`plan_target`. Календарь: `cal_on`, `cal_url` (секрет), `cal_remind`. Google: `g_on`,
`g_client_id`, `g_client_secret` (секрет), `g_refresh` (секрет), `g_read`, `g_write`, `g_list`.
Служебное: `config_version`, `onboarded`, `panel_pin` (меняется только командой `pin`),
`report_logs`, `extra` (неизвестные поля). Все секреты: маскируются в статусе, «маска» при
сохранении = «без изменений», вырезаются из отчётов (`report::secrets`).

## 8. Тесты (что и как запускать)

| Что | Команда | Итог |
|---|---|---|
| Юнит-тесты | `cargo test` | 124/124 |
| `install.sh` (обновление, откат, порча, ротация копий) | `sh tests/install-test.sh` | 20/20 |
| Багрепорт E2E (демон → очередь → stats.py → фейковый Telegram) | `cargo build && python3 tests/report_e2e.py` | 7/7 этапов |
| Планировщик + Google (фейковый Google: OAuth/PKCE, Calendar, Tasks) | `cargo build && python3 tests/planner_e2e.py` | 43 проверки |
| Установщик с настоящим `install.sh` (фейковая колонка `--exec`, SIGHUP как у adbd) | `cargo build && (cd installer && cargo build) && python3 tests/installer_e2e.py` | 10 проверок |
| Бот сообщества (фейковый Bot API) | `python3 tests/community_bot_test.py` | проходит |
| Фейки для ручной проверки | `tests/fake_station.py`, `tests/fake_google.py` | — |
| Панель | Playwright (скрипты были в сессии, не в репо): 390 и 1280 px, без ошибок JS, без горизонтальной прокрутки | ✅ |

Ключевые юнит-тесты: `alice::tests::priority_table`, `planner::tests::day_week_and_deadlines`
(дословный текст сводок), `google::tests::*` (PKCE RFC 7636), `calendar::tests::*`,
`report::tests::secrets_never_survive`, `settings::tests::*`, `eq::tests::*`, `led::tests::*`.

## 9. Что НЕ проверено (честно)

- **Ничего не запускалось на настоящей колонке** в этой работе: обновление/откат, звук, EQ на
  слух, кольцо, экран-часы, голосовые фразы, AirPlay. Чек-лист — `RELEASE_0.5.00_TESTS.md` §B (B1–B9, B8a–B8c).
- **Mac-приложение не запускалось на Mac**: окно через objc runtime, показ фона DMG в Finder,
  запрос «Локальная сеть». Бандл **не подписан целиком** (ad-hoc подписан только arm64-срез
  линкером): возможно «повреждено» → `xattr -cr /Applications/soundya.app` или
  `scripts/mac-dmg/sign-on-mac.sh`. Попытка собрать `rcodesign` с crates.io была запрещена фильтром.
- Windows-установщик и PowerShell-агент не запускались на Windows.
- Настоящий Google (вход, 7-дневное истечение в режиме Testing), настоящий Telegram,
  доступ к SoundCloud из среды — не проверены (только фейки).
- Гипотеза о причине «зависания на 95 %» подтверждена имитацией, не на колонке;
  попросили у владельца `adb shell cat /tmp/soundya-install.log` для подтверждения.

## 10. Что осталось / следующие шаги

1. **Mac-приложение после установки** — остаться в строке меню (трей) с управлением
   музыкой и виджетами (WidgetKit требует Swift/Xcode → проект + сборка на Mac или в
   GitHub Actions macOS). Владелец этого явно хочет.
2. **Windows**: отдельное нативное приложение (трей, мини-плеер, установка в окне без
   браузера, напр. WebView2/Win32). Виджеты Windows 11 требуют MSIX — не делали.
3. Подпись/нотаризация Mac (Developer ID) и подпись Windows — нужны сертификаты владельца.
4. Приёмка на колонках по `RELEASE_0.5.00_TESTS.md` §B; затем версия `0.5.0` в `Cargo.toml`,
   сборка, и **только с разрешения** — релиз.
5. Bluetooth A2DP / AirPlay 2 / Audio Hub — по результатам «Проверить возможности колонки» с каждой модели.
6. Деплой `server/stats.py` и `server/community-bot.py` на сервер автора.
7. Прогон фраз из реальных логов (`voice: …`) против `docs/voice-priority.md`.

## 11. Сборка

- Колонка: `sh scripts/build-mac.sh` (на Linux тоже; zig + cargo-zigbuild,
  target `armv7-unknown-linux-gnueabihf.2.23`) → `dist/soundya-armv7.tar` (VERSION + SHA256SUMS).
  Альтернатива с тулчейном Linaro: `scripts/build-arm.sh`.
- Установщики + DMG: `sh scripts/build-installer.sh` → `dist/soundya-setup-windows.exe`,
  `dist/soundya.dmg`, `dist/soundya-setup-mac.zip` (нужны rustup targets
  x86_64-pc-windows-gnu, aarch64/x86_64-apple-darwin; zig; xorriso; python Pillow;
  `ds_store`+`mac_alias` ставятся в `target/pydeps`).
- Ручная установка: `adb push dist/soundya-armv7.tar /tmp/ && adb shell "cd /tmp && tar -xf soundya-armv7.tar && sh soundya/install.sh"`.
- Хост-запуск для разработки: `SOUNDYA_DIR=… SOUNDYA_RUN=… SOUNDYA_NO_VOICE=1 SOUNDYA_WEB_PORT=18765 target/debug/soundya daemon`.
  Переопределения для тестов: `SOUNDYA_LEDS_ROOT`, `SOUNDYA_PROBE_ROOT`, `SOUNDYA_GOOGLE_AUTH/OAUTH/API`,
  `SOUNDYA_TG_API` (бот), `SOUNDYA_INSTALLER_TAR`, `SOUNDYA_INSTALLER_NO_BROWSER`.

## 12. Как забрать код из архива

```bash
git clone soundya-full.bundle soundya && cd soundya && git checkout release-0.5.00
```
или в существующий клон:
```bash
git fetch /путь/soundya-full.bundle release-0.5.00:release-0.5.00
```
