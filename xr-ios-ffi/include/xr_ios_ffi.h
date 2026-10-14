/* C FFI мост xr-ios-ffi: Network Extension и движок xr-core.
 *
 * Владение строками: функция возвращает char* (UTF-8), владелец Swift,
 * освобождается парной xr_free_string. Пакетный путь едет явным буфером.
 *
 * Заголовок держится руками в двух копиях: include/ у крейта и
 * macos/XrCore/Sources/CXrFfi/include у SwiftPM. Перегенерации нет;
 * совпадение копий и сигнатур держит тест headers_of_bridge_are_identical
 * и покрытие guard.rs (каждая входная точка через ffi_entry!).
 */

#ifndef XR_IOS_FFI_H
#define XR_IOS_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Собрать JSON конфига движка из профиля (ClientProfile). Возвращает JSON
 * конфига с ключами ne_profile и block_quic. Ошибка уезжает JSON {"error":...}. */
char *xr_build_config(const char *setup_json);

/* Проверить конфиг без запуска. "ok" или JSON {"error":...}. */
char *xr_validate_config(const char *config_json);

/* Запустить движок. NULL = успех, иначе текст ошибки для пользователя.
 * Конфиг всегда уходит с ne_profile и block_quic, кроме явно заданных. */
char *xr_start(const char *config_json);

/* Остановить движок и освободить ресурсы сессии. */
void xr_stop(void);

/* Состояние туннеля строкой (то же, что VpnState в ядре). */
char *xr_get_state(void);

/* Статистика JSON: байты, соединения, счётчики, quic_dropped, активный сервер. */
char *xr_get_stats(void);

/* Пакет от устройства в туннель (inbound). */
void xr_push_packet(const uint8_t *data, size_t len);

/* Пакет из туннеля к устройству (outbound). Возвращает длину записанного
 * или -1, если пакета нет / буфер мал. */
ptrdiff_t xr_pop_packet(uint8_t *buf, size_t cap);

/* Сменилась сеть (Wi-Fi <-> LTE). Пересобирает mux-пул. */
void xr_on_network_changed(void);

/* Пропал аплинк. Пул замораживается (XR-296). */
void xr_on_uplink_lost(void);

/* Аплинк вернулся. */
void xr_on_uplink_restored(void);

/* Освободить строку, возвращённую любой функцией моста. */
void xr_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* XR_IOS_FFI_H */
