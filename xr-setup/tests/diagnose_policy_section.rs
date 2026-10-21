//! Стенд секции каналов и политик XR-310 в scripts/diagnose.sh. Скрипт
//! живёт на BusyBox sh роутера, юнитов внутри него нет, поэтому стенд
//! проверяет сам файл: синтаксис целиком и выборку интерфейсов каналов
//! тем же awk, что стоит в скрипте (программа вырезается из его строки,
//! а не дублируется тут).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/diagnose.sh")
}

#[test]
fn script_passes_sh_syntax_check() {
    let out = Command::new("sh")
        .arg("-n")
        .arg(script_path())
        .output()
        .expect("sh на машине стенда");
    assert!(
        out.status.success(),
        "sh -n упал: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn section_ten_lists_wan_interfaces_and_policy_decisions() {
    let text = std::fs::read_to_string(script_path()).unwrap();
    assert!(
        text.contains("== 10. Каналы и политики (XR-310) =="),
        "заголовок секции 10 на месте"
    );
    // Журнальный греб секции обязан ловить строку решения из proxy.rs
    // ("device '...' rule ... exit '...'") и отказ fail-closed.
    assert!(text.contains("exit '|exit pool missing"));
}

#[test]
fn wan_interface_extraction_takes_only_wan_sections() {
    let text = std::fs::read_to_string(script_path()).unwrap();
    let line = text
        .lines()
        .find(|l| l.contains("WAN_IFACES=$(awk"))
        .expect("в diagnose.sh есть выборка интерфейсов каналов");
    let prog = line
        .split("awk '")
        .nth(1)
        .and_then(|rest| rest.split("' ").next())
        .expect("программа awk в одинарных кавычках");

    // Конфиг с двумя каналами и секциями вокруг: выборка обязана отдать
    // оба интерфейса и не взять лишнего из соседних секций.
    let config = concat!(
        "[[servers]]\n",
        "name = \"aeza\"\n",
        "address = \"198.51.100.10\"\n",
        "port = 8443\n",
        "\n",
        "[[wan]]\n",
        "name = \"second\"\n",
        "interface = \"wan2\"\n",
        "\n",
        "[[wan]]\n",
        "name = \"third\"\n",
        "interface = \"wan3\"\n",
        "\n",
        "[[exit]]\n",
        "name = \"second-direct\"\n",
        "wan = \"second\"\n",
        "action = \"direct\"\n",
        "\n",
        "[[device]]\n",
        "name = \"console\"\n",
        "ips = [\"192.0.2.24\"]\n",
        "default_exit = \"second-direct\"\n",
    );
    let mut child = Command::new("awk")
        .arg(prog)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("awk на машине стенда");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(config.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "awk упал: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ifaces = String::from_utf8_lossy(&out.stdout);
    assert_eq!(ifaces.split_whitespace().collect::<Vec<_>>(), ["wan2", "wan3"]);
}
