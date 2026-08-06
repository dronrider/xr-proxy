//! Дверь фронта в хаб и кеш маршрутов (LLD-38 п. 2.3, п. 3.5).
//!
//! Дверь узкая по замыслу: маршрут публикации и вердикт по паролю, оба под
//! общим секретом. Прав админки у фронта нет, приватного ключа хаба он не
//! видит, поэтому взломанный фронт не выпишет себе мандат на агента, которого
//! хаб ему не отдавал.
//!
//! Маршрут кешируется до `exp` минус запас: без кеша каждая картинка страницы
//! стоила бы похода в хаб, а с ним хаб спрашивают не чаще раза в час на
//! публикацию. Хаб недоступен, а кеш ещё жив: публикация продолжает работать.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xr_proto::share::WebRoute;

/// Запас, на который кеш маршрута короче его `exp`: маршрут не должен
/// протухнуть на полпути запроса.
const CACHE_MARGIN_SECS: u64 = 60;

/// Сколько ждём ответа служебной ручки. Браузер стоит на этом ожидании, и
/// висеть на нём минутами хуже, чем назвать отказ.
const HUB_TIMEOUT: Duration = Duration::from_secs(10);

type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Отказ служебной ручки, разобранный по смыслу: «такой публикации нет» это не
/// то же самое, что «хаб не отвечает», и страницы у них разные.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubError {
    /// Публикации нет в реестре хаба.
    NotFound,
    /// Хаб не ответил, ответил не тем или отверг наш секрет.
    Unavailable(String),
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::NotFound => write!(f, "публикация не найдена в реестре хаба"),
            HubError::Unavailable(why) => write!(f, "{why}"),
        }
    }
}

/// Служебные ручки хаба глазами фронта. Отдельным типажом, чтобы тесты гоняли
/// вход и проксирование без единого сетевого вызова.
pub trait HubApi: Send + Sync + 'static {
    fn route(&self, publication: String) -> Boxed<Result<WebRoute, HubError>>;
    /// `Ok(true)` пароль подошёл, `Ok(false)` нет, ошибка это отказ самого
    /// хаба (включая его собственный лимит попыток).
    fn verify_password(&self, username: String, password: String) -> Boxed<Result<bool, HubError>>;
}

/// Живой хаб по HTTPS.
pub struct HttpHub {
    base: String,
    secret: String,
    client: reqwest::Client,
}

impl HttpHub {
    pub fn new(hub_url: &str, secret: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HUB_TIMEOUT)
            .build()
            .map_err(|e| anyhow::anyhow!("http-клиент к хабу: {e}"))?;
        Ok(Self {
            base: hub_url.trim_end_matches('/').to_string(),
            secret: secret.to_string(),
            client,
        })
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.secret)
    }

    /// Состояние публикаций: фронту оно нужно один раз на старте, чтобы
    /// молчаливо не поднялся сервис с чужим секретом. Возвращает число
    /// публикаций, которые хаб согласился показать.
    pub async fn probe(&self) -> Result<usize, HubError> {
        let resp = self
            .client
            .get(format!("{}/api/v1/web/status", self.base))
            .bearer_auth(&self.secret)
            .send()
            .await
            .map_err(|e| HubError::Unavailable(format!("хаб не ответил: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HubError::Unavailable(format!(
                "хаб ответил {status}: {}",
                body.trim()
            )));
        }
        let list: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| HubError::Unavailable(format!("хаб ответил не тем: {e}")))?;
        Ok(list.len())
    }
}

impl HubApi for HttpHub {
    fn route(&self, publication: String) -> Boxed<Result<WebRoute, HubError>> {
        let req = self
            .post("/api/v1/web/route")
            .json(&serde_json::json!({ "publication": publication }));
        Box::pin(async move {
            let resp = req
                .send()
                .await
                .map_err(|e| HubError::Unavailable(format!("хаб не ответил: {e}")))?;
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(HubError::NotFound);
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(HubError::Unavailable(format!(
                    "хаб ответил {status}: {}",
                    body.trim()
                )));
            }
            resp.json::<WebRoute>()
                .await
                .map_err(|e| HubError::Unavailable(format!("маршрут от хаба не разобрался: {e}")))
        })
    }

    fn verify_password(&self, username: String, password: String) -> Boxed<Result<bool, HubError>> {
        let req = self
            .post("/api/v1/web/verify-password")
            .json(&serde_json::json!({ "username": username, "password": password }));
        Box::pin(async move {
            let resp = req
                .send()
                .await
                .map_err(|e| HubError::Unavailable(format!("хаб не ответил: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(HubError::Unavailable(format!(
                    "хаб ответил {status}: {}",
                    body.trim()
                )));
            }
            #[derive(serde::Deserialize)]
            struct Verdict {
                ok: bool,
            }
            resp.json::<Verdict>()
                .await
                .map(|v| v.ok)
                .map_err(|e| HubError::Unavailable(format!("вердикт хаба не разобрался: {e}")))
        })
    }
}

/// Кеш маршрутов публикаций.
pub struct RouteCache {
    hub: Arc<dyn HubApi>,
    entries: Mutex<HashMap<String, Arc<WebRoute>>>,
}

impl RouteCache {
    pub fn new(hub: Arc<dyn HubApi>) -> Self {
        Self {
            hub,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Маршрут публикации: из кеша, пока он свеж, иначе из хаба. Запрос к хабу
    /// идёт вне лока, поэтому медленный хаб не держит остальные публикации.
    pub async fn get(&self, publication: &str, now: u64) -> Result<Arc<WebRoute>, HubError> {
        if let Some(route) = self.fresh(publication, now) {
            return Ok(route);
        }
        let route = Arc::new(self.hub.route(publication.to_string()).await?);
        self.entries
            .lock()
            .expect("route cache lock")
            .insert(publication.to_string(), route.clone());
        Ok(route)
    }

    /// Забыть маршрут: агент оказался не на связи или транзит отвергнут, и
    /// следующий запрос обязан взять маршрут заново, а не долбиться в тот же.
    pub fn forget(&self, publication: &str) {
        self.entries.lock().expect("route cache lock").remove(publication);
    }

    fn fresh(&self, publication: &str, now: u64) -> Option<Arc<WebRoute>> {
        let guard = self.entries.lock().expect("route cache lock");
        let route = guard.get(publication)?;
        (route.exp.saturating_sub(CACHE_MARGIN_SECS) > now).then(|| route.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct CountingHub {
        pub calls: AtomicUsize,
        pub exp: u64,
        pub fail: bool,
    }

    impl HubApi for CountingHub {
        fn route(&self, publication: String) -> Boxed<Result<WebRoute, HubError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (exp, fail) = (self.exp, self.fail);
            Box::pin(async move {
                if fail {
                    return Err(HubError::Unavailable("хаб не ответил: connect refused".into()));
                }
                Ok(crate::test_support::route_for(&publication, exp))
            })
        }

        fn verify_password(&self, _u: String, _p: String) -> Boxed<Result<bool, HubError>> {
            Box::pin(async { Ok(true) })
        }
    }

    fn cache(exp: u64, fail: bool) -> (RouteCache, Arc<CountingHub>) {
        let hub = Arc::new(CountingHub {
            calls: AtomicUsize::new(0),
            exp,
            fail,
        });
        (RouteCache::new(hub.clone()), hub)
    }

    #[tokio::test]
    async fn cached_route_is_reused_and_refreshed_after_exp() {
        let (cache, hub) = cache(10_000, false);
        cache.get("dash", 1_000).await.unwrap();
        cache.get("dash", 1_001).await.unwrap();
        assert_eq!(hub.calls.load(Ordering::SeqCst), 1, "второй запрос идёт из кеша");

        // За минуту до exp маршрут уже считается протухшим: запас в том и
        // состоит, чтобы соединение не открылось по мандату, который умрёт
        // на полпути.
        cache.get("dash", 10_000 - CACHE_MARGIN_SECS).await.unwrap();
        assert_eq!(hub.calls.load(Ordering::SeqCst), 2);

        // Соседняя публикация свой маршрут не наследует.
        cache.get("notes", 1_000).await.unwrap();
        assert_eq!(hub.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn forget_drops_the_entry() {
        let (cache, hub) = cache(10_000, false);
        cache.get("dash", 1_000).await.unwrap();
        cache.forget("dash");
        cache.get("dash", 1_000).await.unwrap();
        assert_eq!(hub.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn hub_failure_is_named_and_not_cached() {
        let (cache, hub) = cache(10_000, true);
        let err = cache.get("dash", 1_000).await.unwrap_err();
        assert!(matches!(err, HubError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("хаб не ответил"));
        cache.get("dash", 1_000).await.unwrap_err();
        assert_eq!(hub.calls.load(Ordering::SeqCst), 2, "отказ не кешируется");
    }
}
