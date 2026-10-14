# Подпись и нотаризация NE-приложения macOS

Системное расширение (Network Extension) ставится на чужую машину только
подписанным и нотаризованным. Без учётки Apple Developer Program приложение
работает только на той машине, где собрано.

## Что нужно

- Учётка Apple Developer Program с ролью Agent или Admin.
- Xcode (полный, не Command Line Tools) с вошедшей учёткой.
- `xcodegen` (ставится `brew install xcodegen`).
- Пароль приложения для нотаризации (appleid.apple.com, раздел
  Sign-In and Security, App-Specific Passwords).

## Идентификаторы и группы

В `macos/XrMac/project.yml` стоят два бандла. Первый, `dev.xr.macos`,
это приложение. Второй, `dev.xr.macos.tunnel`, это расширение Packet
Tunnel Provider.

Оба делят App Group `group.dev.xr.macos`. Через неё приложение кладёт
конфиг, а расширение его читает. При смене префикса бандла поправь
`bundleIdPrefix` в project.yml и group в entitlements обоих таргетов.

В Apple Developer Portal заведи App ID для приложения, App ID для
расширения, App Group `group.dev.xr.macos` и provisioning profile
(Developer ID) для каждого бандла. У обоих App ID нужны capability
App Groups и Network Extension.

## Сборка

Скрипт `scripts/build-macos.sh` собирает staticlib ядра, генерирует
Xcode-проект и гоняет xcodebuild.

```sh
# Debug без подписи, для локальной проверки
scripts/build-macos.sh

# Release с Developer ID подписью
DEVELOPER_ID="Developer ID Application: Имя (TEAMID)" \
  scripts/build-macos.sh --release --sign

# Полный цикл с нотаризацией
DEVELOPER_ID="Developer ID Application: Имя (TEAMID)" \
APPLE_ID="you@example.com" \
APPLE_TEAM_ID="TEAMID" \
APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx" \
  scripts/build-macos.sh --release --sign --notarize
```

Секреты уезжают в переменные окружения. Без этого они оседают в истории
оболочки и в файлах репозитория.

## Что делает скрипт подписи

Скрипт подписывает расширение с entitlements `XrMacTunnel.entitlements`,
затем приложение с `XrMac.entitlements`. Результат проверяется командой
`codesign --verify --deep --strict`. При флаге `--notarize` он упаковывает
.app в zip, отправляет архив командой `notarytool submit --wait` и
прикрепляет `stapler staple`.

Флаг `--options runtime` включает Hardened Runtime. Без него расширение
не пройдёт нотаризацию.

## Проверка на чужой машине

После нотаризации скопируй `.app` на чужой Mac и запусти. При первом
запуске система попросит подтвердить расширение в Системных настройках,
раздел Сеть и фильтры. Подписанный бинарь проходит без предупреждений Gatekeeper.

## Если подпись не удалась

Причина `errSecInternalComponent` означает, что Xcode не вошёл в учётку
или истёк provisioning profile. Сообщение про отсутствие hardened runtime
убирается добавлением `--options runtime`. Отказ про designated requirement
означает, что entitlements не совпадают с provisioning profile. Сверь
App ID и capability. Нотаризация со статусом Invalid пишет причину в лог
notarytool. Частые причины: отсутствие hardened runtime и неподписанный
вложенный бинарь.

## Где живут секреты

Адреса, идентификаторы команд и пароли не в репозитории. Пароль приложения
держи в связке ключей (`xcrun notarytool store-credentials`) или передавай
переменными окружения при запуске скрипта.
