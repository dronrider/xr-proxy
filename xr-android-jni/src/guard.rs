//! Граница JNI против паник ядра (XR-220).
//!
//! Либа живёт в процессе Android-приложения, и паника, добежавшая до
//! extern-функции, убивает весь процесс вместе с работающим VpnService.
//! Профиль сборки `android-release` включает раскрутку стека, а каждая
//! входная точка ловит панику до возврата в JNI: Java-сторона получает
//! оговорённый запасной ответ, сама паника уходит в трейсинг и журнал.
//! Входные точки объявляются только макросом `jni_entry!`, голая
//! extern-функция в lib.rs ловится тестом покрытия.

use std::any::Any;

fn short_name(entry: &str) -> &str {
    entry
        .strip_prefix("Java_com_xrproxy_app_jni_NativeBridge_")
        .unwrap_or(entry)
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "паника без текста".to_string()
    }
}

pub(crate) fn log_jni_panic(entry: &str, payload: &(dyn Any + Send)) {
    let msg = format!("паника в {}: {}", short_name(entry), panic_message(payload));
    tracing::error!("{}", msg);
    crate::journal_log("ERROR", "jni", &msg);
}

/// Объявить входную точку JNI под защитой от паники. Запасной ответ стоит
/// до `fn` и вычисляется только когда паника случилась. Тело пишется как в
/// обычной функции: ранние `return` внутри замыкания возвращают значение из
/// него же, семантика не меняется.
macro_rules! jni_entry {
    ($fallback:expr; fn $name:ident $args:tt -> $ret:ty { $($body:tt)* }) => {
        #[no_mangle]
        pub extern "system" fn $name $args -> $ret {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> $ret {
                $($body)*
            })) {
                Ok(value) => value,
                Err(payload) => {
                    $crate::guard::log_jni_panic(stringify!($name), &*payload);
                    $fallback
                }
            }
        }
    };
    (fn $name:ident $args:tt { $($body:tt)* }) => {
        #[no_mangle]
        pub extern "system" fn $name $args {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                $($body)*
            })) {
                Ok(()) => (),
                Err(payload) => {
                    $crate::guard::log_jni_panic(stringify!($name), &*payload);
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    jni_entry!(-1i32; fn probe_ok(left: i32, right: i32) -> i32 { left + right });
    jni_entry!(-1i32; fn probe_panics() -> i32 { panic!("ядро сломалось") });
    jni_entry!(fn probe_unit_panics() { panic!("без возврата") });
    jni_entry!(-1i64; fn probe_block_on_panics() -> i64 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { panic!("футер сломался") })
    });

    #[test]
    fn panic_does_not_leave_the_entry_point() {
        assert_eq!(probe_ok(20, 22), 42);
        assert_eq!(probe_panics(), -1);
        probe_unit_panics();
    }

    #[test]
    fn block_on_panic_is_caught_by_the_same_boundary() {
        assert_eq!(probe_block_on_panics(), -1);
    }

    #[test]
    fn panic_is_written_to_the_journal() {
        assert!(
            crate::JOURNAL.set(xr_core::journal::Journal::memory()).is_ok(),
            "журнал в тестах ставится один раз"
        );
        assert_eq!(probe_panics(), -1);
        let tail = crate::JOURNAL.get().unwrap().tail();
        let last = tail.last().expect("паника не дошла до журнала");
        assert!(last.contains("ERROR"), "строка: {last}");
        assert!(last.contains("probe_panics"), "строка: {last}");
        assert!(last.contains("ядро сломалось"), "строка: {last}");
    }

    /// Инвариант покрытия: во всём lib.rs нет ни одной extern-функции мимо
    /// макроса, и экспорт символа тоже даёт только макрос. Новая входная точка
    /// без защиты не скомпилируется молча: её заметит этот тест.
    #[test]
    fn every_entry_point_is_generated_by_the_macro() {
        let lib = include_str!("lib.rs");
        assert!(
            !lib.contains("pub extern"),
            "голая extern-функция в lib.rs обходит границу паник, входная точка объявляется только jni_entry!"
        );
        assert!(!lib.contains("#[no_mangle]"), "экспорт символа даёт тот же макрос");
        assert!(
            lib.matches("jni_entry!").count() >= 40,
            "макрос должен накрывать все входные точки NativeBridge"
        );
        let guard_src = include_str!("guard.rs");
        assert_eq!(
            guard_src.matches("pub extern \"system\"").count(),
            2,
            "extern-объявления живут только в двух ветках самого макроса"
        );
    }
}
