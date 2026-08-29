use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;

use crate::state::AppState;

/// Liveness (XR-230): живой процесс отвечает «ok» всегда, без аутентификации
/// и без взгляда на состояние. Внешний аптайм-чек и логика переключения на
/// standby раньше дёргали содержательные ручки (пресеты, инвайты), которые
/// не отличают «процесс жив» от «состояние поднято».
pub(crate) async fn liveness() -> &'static str {
    "ok"
}

/// Readiness (XR-230): неготовность держится, пока hydrate не загрузил
/// инвайты, шары и ключ подписи. Слушатель поднимается после hydrate, так
/// что на живом хабе ручка почти всегда отвечает готовностью; отдельный
/// ответ нужен затем, чтобы мониторинг не принимал отзывчивый процесс
/// за поднятое состояние.
pub(crate) async fn readiness(State(state): State<Arc<AppState>>) -> (StatusCode, &'static str) {
    if state.is_ready() {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}
