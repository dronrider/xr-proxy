//! Общий accept-цикл для листенеров.
//!
//! Наивный `listener.accept().await?` внутри цикла кладёт весь листенер по
//! первой же ошибке, а нехватка дескрипторов на всплеске соединений (EMFILE и
//! родня) проходит сама за миллисекунды: прокси при этом умирал для всех сразу
//! и ждал перезапуска снаружи. Здесь ошибка сначала классифицируется, и на
//! проходящей цикл берёт паузу вместо выхода.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

/// Пауза после проходящей ошибки. Дескрипторы за неё успевают освободиться, а
/// цикл не крутится вхолостую на полной скорости и не заливает журнал.
const RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Сколько ошибок подряд терпим, прежде чем считать листенер безнадёжным.
/// Столько же ошибок с паузой это примерно десять секунд без единого принятого
/// соединения; дальше молчаливое кручение вредно, лучше выйти с ошибкой и дать
/// супервизору (procd, systemd) поднять процесс заново.
const MAX_STREAK: u32 = 100;

/// Проходящая ли ошибка accept(), то есть имеет ли смысл пробовать ещё раз.
///
/// Список белый: незнакомый errno считаем фатальным, чтобы поломка вроде
/// закрытого под ногами дескриптора не оборачивалась вечным циклом.
fn is_transient(e: &io::Error) -> bool {
    match e.raw_os_error() {
        Some(code) => matches!(
            code,
            libc::EMFILE
                | libc::ENFILE
                | libc::ENOBUFS
                | libc::ENOMEM
                | libc::ECONNABORTED
                | libc::ECONNRESET
                | libc::EINTR
                | libc::EAGAIN
                | libc::EPERM
                | libc::EPROTO
                | libc::ETIMEDOUT
                | libc::ENETDOWN
                | libc::ENETUNREACH
                | libc::EHOSTUNREACH
        ),
        None => false,
    }
}

/// Крутит accept, отдавая принятые соединения в `handle`.
///
/// `accept` возвращает `None`, когда листенер надо штатно закрыть (сигнал
/// завершения), и тогда цикл отдаёт `Ok(())`. `Err` цикл отдаёт только на
/// фатальной ошибке или когда проходящие ошибки идут подряд без просвета.
/// `name` попадает в журнал, чтобы по строке было видно, чей листенер сбоит.
///
/// `handle` асинхронный, и цикл его дожидается: кому нужен backpressure, тот
/// берёт пермит семафора прямо в нём, и следующее соединение не примется, пока
/// пермит не выдан. Кому не нужен, тот просто спавнит задачу и возвращается.
pub async fn accept_loop<A, AFut, C, H, HFut>(
    name: &str,
    mut accept: A,
    mut handle: H,
) -> io::Result<()>
where
    A: FnMut() -> AFut,
    AFut: Future<Output = io::Result<Option<(C, SocketAddr)>>>,
    H: FnMut(C, SocketAddr) -> HFut,
    HFut: Future<Output = ()>,
{
    let mut streak: u32 = 0;
    loop {
        match accept().await {
            Ok(Some((conn, peer))) => {
                streak = 0;
                handle(conn, peer).await;
            }
            Ok(None) => return Ok(()),
            Err(e) if is_transient(&e) => {
                streak += 1;
                if streak >= MAX_STREAK {
                    tracing::error!("{name} accept failed {streak} times in a row, giving up: {e}");
                    return Err(e);
                }
                tracing::warn!("{name} accept failed ({streak} in a row): {e}");
                tokio::time::sleep(RETRY_PAUSE).await;
            }
            Err(e) => {
                tracing::error!("{name} accept failed fatally: {e}");
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    enum Step {
        /// Принятое соединение (числом, сокет тут не нужен).
        Conn(u32),
        /// Ошибка с этим errno.
        Fail(i32),
        /// Штатное завершение листенера.
        Stop,
    }

    fn peer() -> SocketAddr {
        "10.0.0.1:1234".parse().unwrap()
    }

    /// Прогоняет сценарий и отдаёт исход цикла, принятые соединения и число
    /// обращений к accept. Сценарий, кончившийся раньше времени, отвечает Stop.
    async fn run(script: Vec<Step>) -> (io::Result<()>, Vec<u32>, usize) {
        let queue = RefCell::new(VecDeque::from(script));
        let calls = RefCell::new(0usize);
        let taken = RefCell::new(Vec::new());
        let (queue, calls) = (&queue, &calls);

        let outcome = accept_loop(
            "test",
            move || async move {
                *calls.borrow_mut() += 1;
                match queue.borrow_mut().pop_front().unwrap_or(Step::Stop) {
                    Step::Conn(id) => Ok(Some((id, peer()))),
                    Step::Fail(code) => Err(io::Error::from_raw_os_error(code)),
                    Step::Stop => Ok(None),
                }
            },
            |id, _| {
                taken.borrow_mut().push(id);
                std::future::ready(())
            },
        )
        .await;

        let calls = *calls.borrow();
        (outcome, taken.into_inner(), calls)
    }

    /// Ради этого теста всё и затевалось: нехватка дескрипторов на всплеске
    /// проходит сама, и листенер обязан пережить её. Со старым `accept().await?`
    /// цикл вышел бы с ошибкой, не приняв ни одного соединения.
    #[tokio::test(start_paused = true)]
    async fn transient_error_keeps_listener_alive() {
        let (outcome, taken, _) = run(vec![
            Step::Fail(libc::EMFILE),
            Step::Conn(1),
            Step::Fail(libc::ENOBUFS),
            Step::Conn(2),
            Step::Stop,
        ])
        .await;

        assert!(outcome.is_ok(), "listener died: {outcome:?}");
        assert_eq!(taken, vec![1, 2]);
    }

    /// Между попытками цикл спит: без паузы EMFILE крутил бы accept на полной
    /// скорости, съедая процессор и заливая журнал.
    #[tokio::test(start_paused = true)]
    async fn transient_error_pauses_before_retry() {
        let start = tokio::time::Instant::now();
        let (outcome, _, _) = run(vec![
            Step::Fail(libc::EMFILE),
            Step::Fail(libc::EMFILE),
            Step::Fail(libc::EMFILE),
            Step::Stop,
        ])
        .await;

        assert!(outcome.is_ok());
        assert_eq!(start.elapsed(), 3 * RETRY_PAUSE);
    }

    /// Закрытый под ногами дескриптор сам не починится, и крутить цикл на нём
    /// бессмысленно: выходим с той же ошибкой, процесс поднимет супервизор.
    #[tokio::test(start_paused = true)]
    async fn fatal_error_stops_the_loop() {
        let (outcome, taken, calls) = run(vec![Step::Fail(libc::EBADF), Step::Conn(1)]).await;

        let err = outcome.expect_err("EBADF must not be retried");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        assert!(taken.is_empty());
        assert_eq!(calls, 1, "loop must not touch the listener after a fatal error");
    }

    /// Проходящая ошибка, которая не проходит, тоже должна закончиться выходом,
    /// а не бесконечным молчаливым циклом.
    #[tokio::test(start_paused = true)]
    async fn endless_transient_errors_give_up() {
        let script = (0..MAX_STREAK + 10).map(|_| Step::Fail(libc::EMFILE)).collect();
        let (outcome, _, calls) = run(script).await;

        assert_eq!(
            outcome.expect_err("endless EMFILE must stop the loop").raw_os_error(),
            Some(libc::EMFILE)
        );
        assert_eq!(calls, MAX_STREAK as usize);
    }

    /// Счётчик подряд идущих ошибок сбрасывается принятым соединением: редкие
    /// EMFILE в течение суток не должны копиться до порога и ронять листенер.
    #[tokio::test(start_paused = true)]
    async fn accepted_connection_resets_the_streak() {
        let mut script: Vec<Step> = (0..MAX_STREAK - 1).map(|_| Step::Fail(libc::EMFILE)).collect();
        script.push(Step::Conn(7));
        script.extend((0..MAX_STREAK - 1).map(|_| Step::Fail(libc::EMFILE)));
        script.push(Step::Stop);

        let (outcome, taken, _) = run(script).await;

        assert!(outcome.is_ok(), "streak must reset on a successful accept: {outcome:?}");
        assert_eq!(taken, vec![7]);
    }
}
