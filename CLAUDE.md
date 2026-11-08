# soundya — правила работы

Подробности об устройстве: `docs/HANDOFF_0.5.00.md`. Документы `RELEASE_0.5.00_*` — планы и
отчёты, а не доказательство реализации: каждое утверждение сверять с кодом и тестами.

## Жёсткие правила владельца
- **Ничего не отправлять в git (commit, push, tag, release) без явного одобрения владельца.**
- Не выпускать релиз, не публиковать артефакты, не менять версию без отдельного поручения.
- Секреты, ключи, токены, персональные данные — не в код, логи, фикстуры и отчёты.
- Не заявлять поддержку и «проверено» без проверки. Проверку на имитации (фейковая колонка,
  `tests/fake_*.py`, `hdiutil makehybrid` вместо xorriso) так и называть — имитацией.
- Общение по-русски.

## Границы и опасные места
- `src/` — программа на колонке (armv7, Linux, без async; один поток Controller). Не ломать
  HTTP API панели (`web.rs`) и протокол с агентом ПК (`pc.rs` ↔ `pc/soundya-pc.ps1`: HMAC
  `time\nnonce\npath\nbody`, порт 8766) без необходимости.
- `installer/` — отдельный крейт (Windows .exe, macOS .app); переиспользует `src/adb.rs`, `src/fsx.rs`.
- `packaging/install.sh` — обновление с откатом на колонке; ошибка здесь ломает колонку.
- macOS-приложение: окно через objc runtime (`installer/src/macwin.rs`), без SDK.
- DMG: раздавать только образ, собранный на Mac: `sh scripts/build-installer.sh` там сам подписывает
  бандл и собирает UDZO через `hdiutil` (`build_dmg.py::build_mac`, alias фона с тома в /Volumes).
  Образ xorriso с Linux — черновик: после скачивания не монтируется без предупреждения.
- Виджет Mac: `mac-widget/` (Swift, WidgetKit, macOS 14+), собирается `scripts/mac-widget/build.sh`
  (`xcodebuild` проекта `mac-widget/xcode`, нужен полный Xcode; `SOUNDYA_XCODE=/Applications/Xcode-beta.app` — выбрать) и в
  `build-installer.sh` подписывается изнутри наружу (виджет с `widget.entitlements`, потом приложение — без `--deep`).
  Виден только при подписи с Team ID: `SOUNDYA_SIGN_ID="Apple Development: …" sh scripts/build-installer.sh`.
  `swiftc` вручную не годится: нет точки входа `_NSExtensionMain` и `Metadata.appintents`.
  `check_app.py` проверяет и виджет (`python3 tests/check_app_test.py` — тест на поддельных бандлах).
- `scripts/build-mac.sh` начинает с `rm -rf dist` — сохраняйте нужные артефакты заранее (`--dev` трогает только `dist/soundya`).
- Версия разработчика: фича `dev` в обоих крейтах (`src/devpage.rs`, `dev/*.js`, `installer/src/dev.rs`).
  В обычной сборке её кода быть не должно — проверяет `tests/dev_feedback_e2e.js` (второй бинарник).
- Версия: semver в `Cargo.toml` (`0.5.0`), на экранах «0.5.00» (`shownVersion` в мастере, `display_version` в фоне DMG).
- Агент ПК: Windows — `pc/*.ps1`; Mac — `installer/src/agent.rs` (тот же бинарник soundya.app с
  аргументом `agent`, LaunchAgent). Протокол общий с `src/pc.rs`; Mac-агент принимает только подпись.
  Действия Mac-агента идут через `Runner`; `SOUNDYA_PC_DRYRUN=1` — ничего не выполнять, только журнал.
  Никогда не проверять sleep/shutdown/restart/lock/close по-настоящему на машине владельца.

## Проверки (подтверждены в репозитории)
| Что | Команда | Где работает |
|---|---|---|
| Юнит-тесты колонки | `cargo test` | macOS, Linux (124 теста) |
| Юнит-тесты установщика | `(cd installer && cargo test)` | macOS, Linux |
| `.app`: минимальная macOS бинарника ≤ `LSMinimumSystemVersion`, исполняемый файл | `python3 scripts/mac-dmg/check_app.py dist/mac/soundya.app` | везде |
| Сборка .app под macOS | `cd installer && MACOSX_DEPLOYMENT_TARGET=11.0 cargo zigbuild --release --target universal2-apple-darwin` | нужен target x86_64-apple-darwin |
| Доводка DMG | `sh scripts/mac-dmg/sign-on-mac.sh dist/soundya.dmg` | только macOS |
| `install.sh` | `sh tests/install-test.sh` | **на macOS падает (15 проверок, нужен Linux/root); код выхода 0 даже при провалах — читать вывод** |
| E2E | `tests/report_e2e.py`, `tests/planner_e2e.py`, `tests/installer_e2e.py` (после `cargo build`) | имитации; **только Linux**: на macOS `cargo build` не линкуется (`prctl` в `zapret.rs`/`airplay.rs`), хотя `cargo test` проходит |
| Версия разработчика (фидбек) | `cargo test --features dev`; `(cd installer && cargo test --features dev)`; в Chromium: `cargo build --features dev && cp target/debug/soundya /tmp/soundya-dev && cargo build && node tests/dev_feedback_e2e.js /tmp/soundya-dev target/debug/soundya`; мастер: `(cd installer && cargo build --features dev) && node tests/dev_wizard_e2e.js` | Linux (имитация); установщику нужны `dist/soundya-armv7{,-dev}.tar` (хоть пустые для тестов — потом удалить!) |
| `check_app.py` (виджет) | `python3 tests/check_app_test.py` | везде (поддельные бандлы) |
| Протокол колонка ↔ агент ПК | `cargo test pc::tests` (поддельный агент на 127.0.0.1) | везде; сам PowerShell-агент здесь не запускается |
| Mac-агент | `(cd installer && cargo test agent::)`; вживую: `HOME=<tmp> SOUNDYA_PC_DRYRUN=1 installer/target/debug/soundya-installer agent` с `config.json` (свой порт) в `<tmp>/Library/Application Support/soundya-pc/` | macOS |

Требуют настоящего устройства/ОС и всегда помечаются как непроверенные без него: колонка
(звук, голос, LED, обновление), Windows (установщик, агент ПК), первый запуск на чистом Mac
(Gatekeeper помнит одобрения — на машине разработчика не воспроизводится), macOS 11–14.

## Цикл багфикса
1. Переформулировать: ожидаемое vs фактическое поведение.
2. Найти причину в коде; не менять код до понимания причины.
3. Тест, воспроизводящий дефект (если данных хватает); не ослаблять существующие тесты.
4. Минимальное исправление, без попутного рефакторинга и новых зависимостей.
5. Прогнать релевантные проверки, перечитать diff.
6. Отчёт: причина; изменённые файлы; исправление; проверки с командами и результатами;
   что не проверено и где; ручные шаги для владельца.
