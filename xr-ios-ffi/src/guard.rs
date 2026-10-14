//! Граница C FFI против паник ядра.
//!
//! Библиотека живёт в процессе Network Extension, и паника, добежавшая до
//! extern-функции, убивает весь процесс расширения вместе с туннелем.
//! Профиль сборки расширения держит раскрутку стека, а каждая входная точка
//! ловит панику до возврата в Swift: хост получает оговорённый запасной ответ,
//! сама паника уходит в трейсинг. Входные точки объявляются только макросом
//! `ffi_entry!`, голая extern-функция в lib.rs ловится тестом покрытия.

use std::any::Any;

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "паника без текста".to_string()
    }
}

pub(crate) fn log_ffi_panic(entry: &str, payload: &(dyn Any + Send)) {
    let msg = format!("паника в {}: {}", entry, panic_message(payload));
    tracing::error!("{}", msg);
}

// Объявить входную точку C FFI под защитой от паники. Запасной ответ стоит
// до `fn` и вычисляется только когда паника случилась. Тело пишется как в
// обычной функции: ранние `return` внутри замыкания возвращают значение из
// него же, семантика не меняется.
macro_rules! ffi_entry {
    ($fallback:expr; fn $name:ident $args:tt -> $ret:ty { $($body:tt)* }) => {
        #[no_mangle]
        pub extern "C" fn $name $args -> $ret {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> $ret {
                $($body)*
            })) {
                Ok(value) => value,
                Err(payload) => {
                    $crate::guard::log_ffi_panic(stringify!($name), &*payload);
                    $fallback
                }
            }
        }
    };
    (fn $name:ident $args:tt { $($body:tt)* }) => {
        #[no_mangle]
        pub extern "C" fn $name $args {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                $($body)*
            })) {
                Ok(()) => (),
                Err(payload) => {
                    $crate::guard::log_ffi_panic(stringify!($name), &*payload);
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    ffi_entry!(-1i32; fn probe_ok(left: i32, right: i32) -> i32 { left + right });
    ffi_entry!(-1i32; fn probe_panics() -> i32 { panic!("ядро сломалось") });
    ffi_entry!(fn probe_unit_panics() { panic!("без возврата") });
    ffi_entry!(-1i64; fn probe_block_on_panics() -> i64 {
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
                    text.matches("pub extern \"C\"").count(),
                    2,
                    "extern-объявления живут только в двух ветках самого макроса"
                );
            } else {
                assert!(
                    !text.contains("pub extern"),
                    "{name}: голая extern-функция обходит границу паник, входная точка объявляется только ffi_entry!"
                );
                assert!(
                    !text.contains("#[no_mangle]"),
                    "{name}: экспорт символа даёт тот же макрос"
                );
            }
            entries += text.matches("ffi_entry!").count();
        }
        assert!(
            entries >= 8,
            "макрос должен накрывать все входные точки моста"
        );
    }
}
