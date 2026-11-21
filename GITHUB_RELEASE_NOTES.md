# TGWS Rust Proxy v1.0.0 — Android

Локальное Android-приложение для Telegram. Оно поднимает на телефоне MTProto-прокси, а сетевой движок на Rust передаёт трафик через WebSocket.

## О приложении

TGWS Rust Proxy работает на самом устройстве. Telegram подключается к локальному адресу `127.0.0.1:1443`. Дальше приложение открывает защищённое WebSocket-соединение напрямую к дата-центру Telegram или, если прямой маршрут не отвечает, через Cloudflare.

## Возможности

- Включение и выключение прокси с главного экрана.
- Передача настройки в Telegram кнопкой «Применить в Telegram».
- Фоновая служба с уведомлением. Служба остаётся работать после сворачивания приложения.
- Сохранение локального порта после краткого отключения и возврата Wi-Fi.
- Понятные статусы: «Не подключено», «Подключение…», «Подключено», «Восстановление соединения…», «Нет доступа к сети», «Не удалось подключиться».
- Автоматический выбор маршрута по ответам в текущей сети и сохранение ручного порядка.
- Защищённое соединение на порту 443 с проверкой сертификата и имени сервера.
- Режим разработчика с журналом и копированием отчёта. Секреты, токены, ссылки прокси и IP-адреса в отчёте скрываются.
- Русский и английский интерфейс, светлая и тёмная тема.

Режим Cloudflare через порт 80 сам не включается. Он доступен только после явного согласия и только в мобильной сети.

## Скачать

| Файл | Кому подходит |
| --- | --- |
| [proxy-universal-release.apk](https://github.com/wendercurent/TGWS-Rust-Proxy/releases/download/v1.0.0/proxy-universal-release.apk) | Большинству пользователей |
| [proxy-arm64-v8a-release.apk](https://github.com/wendercurent/TGWS-Rust-Proxy/releases/download/v1.0.0/proxy-arm64-v8a-release.apk) | Современным 64-битным ARM-телефонам |
| [proxy-armeabi-v7a-release.apk](https://github.com/wendercurent/TGWS-Rust-Proxy/releases/download/v1.0.0/proxy-armeabi-v7a-release.apk) | Старым 32-битным ARM-телефонам |
| [SHA256SUMS.txt](https://github.com/wendercurent/TGWS-Rust-Proxy/releases/download/v1.0.0/SHA256SUMS.txt) | Проверка скачанных файлов |

## Как установить

1. Скачайте один APK. Если не знаете модель телефона, берите Universal.
2. Откройте скачанный файл и подтвердите установку. Если Android спросит разрешение на установку из браузера или файлового менеджера, разрешите его для этого источника.
3. Откройте приложение TGWS Rust.
4. Нажмите кнопку на главном экране и дождитесь статуса «Подключено».
5. Нажмите «Применить в Telegram» и подтвердите подключение в Telegram.
6. Для выключения нажмите кнопку прокси ещё раз.

## Что нового в v1.0.0

Это первая публичная сборка форка.

- Локальный MTProto-прокси на Rust и экран управления на Jetpack Compose.
- Три подписанные сборки: Universal, ARM64 и ARM32. Минимальная версия Android — 5.0.
- Проверка TLS на порту 443 и запасной маршрут через Cloudflare.
- Фоновая служба, уведомление и простые статусы соединения.
- Режим разработчика и очистка чувствительных данных в отчёте.
- Нативная библиотека ARM64 выровнена по страницам 16 КБ.

## Совместимость

Android 5.0 и новее. Поддерживаются архитектуры ARM64 и ARM32. Номер версии приложения — 1.0.0, номер сборки — 1250. Пакет: `dev.wendercurent.tgws`.

## Безопасность

Все три APK подписаны одним ключом выпуска. Проверить файл можно по SHA-256 из `SHA256SUMS.txt`. Отпечаток сертификата SHA-256: `45696cd175178d3c16856b36621ddad9fd0e3a6c6b2b59435e2d4ea022811259`.

Подпись показывает, что файл собран владельцем этого репозитория. Она не делает соединение или устройство абсолютно защищёнными. Секрет прокси и ссылку `t.me/proxy` не нужно отправлять посторонним.

## Известные ограничения

- ARM64-сборка проверена на физическом смартфоне с Android 13: установка, запуск, включение и выключение прокси, фоновая служба и восстановление после краткого отключения Wi-Fi.
- ARM32-сборка на физическом 32-битном устройстве не проверялась.
- Universal-сборка отдельно на 32-битном устройстве не проверялась.
- Живой обмен сообщениями и загрузка медиа через пакет `dev.wendercurent.tgws` пока не подтверждены.
- Поддержка звонков Telegram не заявляется.

## English

TGWS Rust Proxy is an Android app that runs a local Telegram MTProto proxy. The network engine is written in Rust and uses a WebSocket transport, either directly to a Telegram datacenter over verified TLS on port 443 or through Cloudflare when the direct route does not answer.

Download Universal unless you specifically need the ARM64-only or ARM32-only package. Android 5.0 or newer is required. The ARM64 package was tested on a physical Android 13 phone. The ARM32 package was not tested on a physical 32-bit device, live Telegram messaging through this production package is not yet confirmed, and Telegram calls are not claimed.

## Авторы

Оригинальный прокси — [Flowseal/tg-ws-proxy](https://github.com/Flowseal/tg-ws-proxy), уведомление MIT сохранено. Android-приложение и Rust-код, от которых начат форк, — [amurcanov/tg-ws-proxy-android](https://github.com/amurcanov/tg-ws-proxy-android). Этот репозиторий распространяется под GPL-3.0.
