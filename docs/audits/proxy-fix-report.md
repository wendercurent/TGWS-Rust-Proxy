# Отчёт об исправлении стабильности TGWS Rust Proxy

- **Ветка:** `fix/proxy-stability` (создана от `origin/main` `a6f66f46e4a59a10a0a2eea162c4289af612bd80`)
- **Дата:** 23 ноября 2026
- **Незатронутые области:** `feat/calls` (отдельная рабочая копия `tgws-calls`), `main`, release-подпись, `applicationId`, UI/навигация/анимации, политика TLS (plaintext WS/80 остаётся строго opt-in и запрещён на Wi-Fi).

---

## 1. Исправленные дефекты (по аудиту S1–S9)

### P0.2 (S2) — Единый first-response timeout и корректное протокольное evidence на WS и TCP
- **Root cause:** Обычные WebSocket-сессии и route-only half-open пробы не имели короткого таймаута ожидания первого ответа после отправки клиентского запроса (ждали 120 с `BRIDGE_READ_TIMEOUT`), а `idle_timeout` не записывался как `Evidence::ResponseTimeout`. В TCP-реле (`bridge_tcp_observed`) не было first-response deadline, положительное `Evidence::ProtocolResponse`/`Sustained` выставлялось до подтверждения записи клиенту, а при молчании или раннем EOF upstream отрицательное evidence не передавалось менеджеру маршрутов.
- **Исправление:**
  - Введён `FIRST_RESPONSE_TIMEOUT` (10 с) в `src/config.rs`, запускаемый только после фактической отправки первого клиентского payload upstream (чтобы idle-сессии без клиентского запроса не обрывались преждевременно).
  - Применён единообразно ко всем WS-сессиям (ordinary, route-probe, family-probe) и TCP-реле.
  - В `bridge_ws` добавлена запись `Evidence::ResponseTimeout` на `first_response_timeout`/`idle_timeout` при `!has_response && !transport_error && up_bytes > 0`.
  - В `bridge_tcp_observed` переведена обработка на `try_join!` с различением `UpstreamEof`, `UpstreamRead`, `UpstreamTimeout`, `FirstResponseTimeout` и клиентских ошибок (`ClientWrite`, `ClientBackpressure`). Положительное evidence выставляется строго после успешного `c_write.write_all`.

### P0.3 & P0.1 (S3, S1, S6) — Изоляция DC/ролей в circuit breaker, bounded retry при `Busy` и диагностика исчерпания маршрутов
- **Root cause:**
  1. В `routing::Manager::start` проверка `family.consecutive + family.in_flight >= FAMILY_THRESHOLD` (4) применялась даже при наличии здорового маршрута того же семейства (`has_healthy == true`), из-за чего при `consecutive == 3` любой один `in_flight` dial блокировал все остальные DC и media-роли.
  2. В `RouteUse::observe_at` ошибка уровня MTProto `Evidence::TransportError(_)` (например `-404` для одного DC/роли) передавалась в `finish_family` и могла открыть общий circuit breaker семейства CF/Worker для остальных DC.
  3. В `serve_core` локальная занятость слота (`WsError::Busy`) немедленно пропускала кандидата без повторной попытки, а лог `routes_exhausted` не раскрывал причину `route=None attempt=0`.
- **Исправление:**
  - В `src/routing.rs` введён `SkipReason` (`NetworkChanged`, `RouteCooling`, `RouteProbing`, `FamilyCooling`, `FamilyBudget`).
  - Gate `consecutive + in_flight >= FAMILY_THRESHOLD` применяется только при `!has_healthy`.
  - `Evidence::TransportError(_)` больше не инкрементирует общий family breaker и влияет только на `(Scope, Candidate)`.
  - В `serve_core` добавлен один ограниченный повтор (150 мс, с поддержкой отмены и смены `epoch`) по кандидатам, вернувшим локальный `WsError::Busy`.
  - В `routes_exhausted` (INFO) и в `GetRouteDiagnostics()` (`exhausted[]`) добавлена безопасная структурированная разбивка причин пропуска (`route_cooling`, `route_probing`, `family_cooling`, `family_budget`, `busy`, `dial_failed`, `candidates`, `attempts`) по `(dc, media, test)` без секретов, payload и приватных доменов.

### P0.4 (S7) — Строгая валидация MTProto secret на FFI и атомарная инициализация в Android
- **Root cause:** `StartProxy` при невалидном (пустом/коротком/не-hex) `secret` не возвращал ошибку, молча сохранял старый `PROXY_SECRET` и поднимал listener. В `ProxyController.ensureSecretKey` параллельный первый запуск при пустом DataStore мог создать два разных секрета.
- **Исправление:**
  - `StartProxy` проверяет 32-символьный hex до изменения состояния и до `bind`, возвращая явный код ошибки `-5`.
  - `ProxyController.ensureSecretKey` сериализован через `kotlinx.coroutines.sync.Mutex`.

### P1 (S5) — Ограниченный перебор stale-сокетов в `WsPool` и проактивная очистка старого `epoch`
- **Root cause:** `WsPool::get` проверял только первый сокет из очереди и при неудачном `check_idle()` сразу возвращал miss, даже если за ним в очереди находился рабочий сокет. Сокеты старого сетевого поколения очищались только лениво.
- **Исправление:**
  - `WsPool::get` проверяет до 3 кандидатов в пределах общего бюджета 1200 мс без удержания queue lock на время сетевого I/O.
  - Добавлен `WsPool::evict_stale_epoch(epoch)`, вызываемый из `NotifyDefaultNetworkState`, для немедленного удаления сокетов и `refill_failures` предыдущего сетевого поколения.

### P1 (S4, S9) — Восстановление по `NET_CAPABILITY_VALIDATED` и безопасный цикл остановки `ProxyService`
- **Root cause:** `ProxyService.reportDefaultNetwork()` не отслеживал `NET_CAPABILITY_VALIDATED`, поэтому при восстановлении WAN на том же Wi-Fi (`false -> true`) нативные cooldown и stale idle-сокеты не сбрасывались. В `stopProxy()` флаг `stopInProgress` не сбрасывался после завершения остановки, что могло блокировать повторный старт до `onDestroy()`.
- **Исправление:**
  - Вынесена тестируемая политика `NetworkSnapshotPolicy` (`NetworkSnapshot`, `NetworkTransition`). Переход `validated: false -> true` на том же `Network` вызывает `NotifyDefaultNetworkState(..., replaced = false)` — продвигает `epoch` (сбрасывая cooldown/pool), но не рвёт здоровые активные relay. Смена `Network` по-прежнему передаёт `replaced = true` (handover). Дубликаты callback игнорируются.
  - `stopProxy()` гарантированно сбрасывает `stopInProgress` в блоке `finally` и логирует ситуацию, если нативный stop превысил 3-секундный бюджет ожидания.

---

## 2. Оценка HTTP/2 Media Transport (Flowseal)

**Решение: HTTP/2 media transport в этот PR не добавляется.**

Обоснование по данным аудита (`docs/audits/device-test-results.md`, `docs/audits/flowseal-comparison.md`, `docs/PROGRESS.md`):
1. На целевом Android-устройстве (TECNO CI8n, Wi-Fi) все 4 пробы CF H2 ALPN (`+1/slot0`, `+2/slot0`, `+2/slot1`, `+5/slot0`) завершились **TLS timeout за 4.05–4.25 с** — до достижения HTTP/2 или `POST /api`.
2. В самом эталоне Flowseal (`3fc8b9ee9`) 3 сценария `H2Wire` в `tests/test_h2_transport.py` не прошли локальную проверку.
3. Добавление `h2` в data plane без подтверждённо доступного upstream на проблемной Wi-Fi сети не устраняет TLS-таймаут, добавляет тяжёлую зависимость и риск регрессии replay/flow-control.

---

## 3. Изменённые файлы и список коммитов

Коммиты на ветке `fix/proxy-stability`:
1. `fa5ffc7` — `fix(proxy): enforce first-response timeout and protocol evidence on WS and TCP`
2. `4447339` — `fix(routing): isolate DC failures, retry Busy slots, and record exhaustion reasons`
3. `1591562` — `fix(secret): reject invalid FFI secret before bind and serialize Controller init`
4. `e1b09b8` — `fix(pool): scan past stale sockets within checkout budget and evict old network epochs`
5. `d5a5626` — `fix(android): recover on NET_CAPABILITY_VALIDATED and harden service stop lifecycle`
6. *(текущий коммит)* — `docs(audits): add proxy stability fix report`

Изменённые и добавленные исходные файлы:
- `src/config.rs`
- `src/lib.rs`
- `src/proxy.rs`
- `src/routing.rs`
- `app/src/main/java/com/amurcanov/tgwsproxy/NetworkSnapshotPolicy.kt` *(новый)*
- `app/src/main/java/com/amurcanov/tgwsproxy/ProxyController.kt`
- `app/src/main/java/com/amurcanov/tgwsproxy/ProxyService.kt`
- `app/src/test/java/com/amurcanov/tgwsproxy/NetworkSnapshotPolicyTest.kt` *(новый)*
- `app/src/test/java/com/amurcanov/tgwsproxy/ProxyControllerSecretTest.kt` *(новый)*
- `docs/audits/proxy-fix-report.md` *(новый)*

---

## 4. Результаты тестов и сборок

| Проверка | Команда | Результат |
| --- | --- | --- |
| Rust unit & integration suite | `cargo test --locked -- --test-threads=1` | **PASS** (exit 0: **69 passed, 0 failed, 11 ignored**) |
| Isolated sticky-listener TLS loopback test | `cargo test --locked -- --ignored sticky_listener_pins_one_init_and_rejects_cross_epoch_upgrade --nocapture` | **PASS** (exit 0: **1 passed, 0 failed**, 0.83 s) |
| Rust Clippy | `cargo clippy --all-targets` | **PASS с предупреждениями** (exit 0; 26 унаследованных warnings, 0 новых ошибок) |
| Rust Format check | `cargo fmt --check` | **FAIL (унаследовано)** (exit 1; массовое переформатирование нетронутых файлов не выполнялось) |
| Android JVM unit tests | `.\gradlew.bat :app:testArm64DebugUnitTest --no-daemon` | **PASS** (exit 0: **17 tests, 0 failures, 0 errors** — `LogPrivacyTest` 2, `NetworkSnapshotPolicyTest` 5, `ProxyControllerSecretTest` 2, `ProxySessionTrafficTest` 3, `DomainListTest` 1, `Ws80SuggestionPolicyTest` 4) |
| Native ARM64 Rust build | `cmd /c build_so.bat --arm64-only` (`cargo build --release --locked --target aarch64-linux-android`) | **PASS компиляция** (артефакт собран в `$CARGO_TARGET_DIR\aarch64-linux-android\release\libtgwsproxy.so` и скопирован в `build\generated\tgwsNative\arm64-v8a\libtgwsproxy.so`) |
| Android Debug APK | `.\gradlew.bat :app:assembleArm64Debug --no-daemon` | **PASS** (exit 0) |

### Контрольные суммы артефактов сборки
- Fresh ARM64 `.so` (`build\generated\tgwsNative\arm64-v8a\libtgwsproxy.so`): `2,279,720` bytes, SHA-256 `0F6C265890A231C89E6B04F4BCB8FA2304E4D6E0A0F857A033A3704134E8324B`
- Embedded `lib/arm64-v8a/libtgwsproxy.so` внутри APK: SHA-256 `0F6C265890A231C89E6B04F4BCB8FA2304E4D6E0A0F857A033A3704134E8324B` (**совпадение побайтово: `True`**)
- Debug APK (`app\build\outputs\apk\arm64\debug\app-arm64-debug.apk`): `19,248,886` bytes, SHA-256 `BA9C5439D1ED16E9C2E422C2A19DC996A7EE6BC1D42CD3A643611F5770655E42`

---

## 5. Оставшиеся ограничения и внешние факторы

1. **Сетевая недоступность DC1 / CF TLS на конкретном Wi-Fi:** Как доказано независимыми пробами аудита (`device-test-results.md`), на тестовом Wi-Fi (TECNO CI8n) публичные named WSS для DC1, TCP/443 и проверенные CF WSS кандидаты завершались таймаутом на уровне сети/TLS, а общий direct IP `149.154.167.220` не валидирует TLS hostname для `kws1.web.telegram.org`. Локальные исправления маршрутизации не обходят внешнюю сетевую блокировку DC1 без доступного внешнего relay/Worker, обслуживающего DC1.
2. **Живой прогон на физическом устройстве (T1–T12):** Новая сборка не устанавливалась на телефон пользователя (согласно правилу не перезаписывать установленный прокси без явного разрешения). Требуется независимая перепроверка на устройстве: переключение A/B, загрузка медиа по Wi-Fi, циклы Wi-Fi↔LTE, потеря/восстановление WAN на том же Wi-Fi и длительный soak.
3. **`armeabi-v7a`:** В этом цикле проверялась сборка `arm64-v8a`; `armeabi-v7a` не собирался.
4. **Примечание по `build_so.bat`:** Если в окружении задана переменная `CARGO_TARGET_DIR`, скрипт `build_so.bat` ожидает выходной `.so` в относительном `target\...`. В стандартной среде без `CARGO_TARGET_DIR` скрипт копирует файл автоматически.

---

## 6. Риски регрессии

- **First-response timeout (10 с):** На экстремально медленных сетях с RTT > 10 с до первого MTProto-кадра после отправки запроса сокет может закрыться раньше ответа и инициировать переподключение Telegram. Порог выбран по рекомендации аудита (P0.2: 8–10 с) и не срабатывает на idle-соединениях без отправленного запроса.
- **`StartProxy` error `-5`:** Внешние/нештатные вызовы FFI с пустым или битым secret теперь завершаются отказом вместо тихого использования старого ключа; штатный путь через `ProxyController` всегда передаёт валидный 32-hex ключ.
- **Revalidation (`VALIDATED false -> true`):** Продвижение `epoch` без `handover` сохраняет работающие relay и очищает idle-пул/cooldown; кратковременный всплеск новых dial-попыток при восстановлении WAN ограничен существующими семафорами `dial.rs`.

## 7. Готовность к независимой перепроверке

Ветка `fix/proxy-stability` готова к ревью и независимой перепроверке на физическом устройстве с помощью `app-arm64-debug.apk` (отдельный debug `applicationId`, не затрагивает production-пакет и `feat/calls`).
