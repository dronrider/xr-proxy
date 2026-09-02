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
    // Журнал один на процесс, и тесты бинаря идут параллельно: этот probe
    // зовёт только сам тест журнала, чтобы искать в хвосте свою строку, а не
    // чужую от соседа, тоже паниковавшего через probe_panics.
    jni_entry!(-1i32; fn probe_journal_panics() -> i32 { panic!("журнальная паника") });
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
        assert_eq!(probe_journal_panics(), -1);
        let tail = crate::JOURNAL.get().unwrap().tail();
        let last = tail
            .iter()
            .rev()
            .find(|line| line.contains("probe_journal_panics"))
            .expect("паника не дошла до журнала");
        assert!(last.contains("ERROR"), "строка: {last}");
        assert!(last.contains("журнальная паника"), "строка: {last}");
    }

    /// Инвариант покрытия: ни в одном исходнике крейта нет extern-функции мимо
    /// макроса, и экспорт символа тоже даёт только макрос. Тест обходит все
    /// исходники src целиком, поэтому входная точка в новом модуле крейта без
    /// защиты не останется незамеченной.
    #[test]
    fn every_entry_point_is_generated_by_the_macro() {
        fn collect_rust(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("каталог src читается") {
                let entry = entry.expect("чтение записи каталога");
                let path = entry.path();
                if path.is_dir() {
                    collect_rust(&path, out);
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".rs") {
                    let text = std::fs::read_to_string(&path).expect("исходник читается");
                    out.push((name, text));
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        collect_rust(&src, &mut sources);
        sources.sort();
        assert!(sources.len() >= 2, "в обход должны попасть все исходники крейта");
        let mut entries = 0;
        for (name, text) in &sources {
            if name == "guard.rs" {
                assert_eq!(
                    text.matches("pub extern \"system\"").count(),
                    2,
                    "extern-объявления живут только в двух ветках самого макроса"
                );
            } else {
                assert!(
                    !text.contains("pub extern"),
                    "{name}: голая extern-функция обходит границу паник, входная точка объявляется только jni_entry!"
                );
                assert!(
                    !text.contains("#[no_mangle]"),
                    "{name}: экспорт символа даёт тот же макрос"
                );
            }
            entries += text.matches("jni_entry!").count();
        }
        assert!(
            entries >= 40,
            "макрос должен накрывать все входные точки NativeBridge"
        );
    }
}
