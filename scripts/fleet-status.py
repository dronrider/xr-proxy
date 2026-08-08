#!/usr/bin/env python3
"""Сводка по флоту одной командой (XR-113): что за сборки стоят на машинах, живы
ли сервисы, куда выходят роутеры и одинаковый ли релиз отдают оба хаба.

Нужна после выката и в инциденте, когда вопрос звучит «а точно ли там то, что я
выкатил» и «кто из машин отвалился». Каждая машина опрашивается одним ssh, все
разом, ответ разбирается в таблицу и в список проблем. Недоступная машина не
молчит: она попадает в сводку строкой «НЕДОСТУПНА» с причиной от ssh, уходит в
список проблем и роняет код возврата.

  python3 scripts/fleet-status.py
  python3 scripts/fleet-status.py --only router-ru,hub-primary
  python3 scripts/fleet-status.py --json

Версии бинарей брать неоткуда: `--version` есть только у xr-setup, поэтому
личность сборки это md5 файла. Сам по себе md5 ни о чём не говорит, работает он
сравнением: одинаковый ли бинарь на двух хабах, тот ли, что вчера, сходится ли
он с только что собранным. Расхождение по одному имени бинаря между машинами
попадает в проблемы.

Адреса флота в репозиторий не едут, они лежат в гитигнорнутом
local-docs/fleet.ini (путь переопределяется FLEET_CONF); запуск без него
печатает готовый шаблон и останавливается.

Зависимостей нет, только стандартная библиотека: скрипт гоняется и с макбука
владельца, и с любой машины флота.
"""

import argparse
import concurrent.futures
import configparser
import datetime
import io
import json
import os
import shlex
import subprocess
import sys

DEFAULT_CONF = "local-docs/fleet.ini"
TRACE_URL = "https://www.cloudflare.com/cdn-cgi/trace"

# Что опрашивается у машины роли, если секция не сказала иначе.
ROLE_DEFAULTS = {
    "hub": {"units": ["xr-hub"], "binaries": ["/usr/local/bin/xr-hub"], "nft_tables": []},
    "server": {
        "units": ["xr-proxy-server"],
        "binaries": ["/usr/local/bin/xr-server"],
        "nft_tables": [],
    },
    "router": {"units": [], "binaries": ["/usr/bin/xr-client"], "nft_tables": ["xr_proxy"]},
}

CONF_TEMPLATE = """\
# Флот xr-proxy для scripts/fleet-status.py. Файл гитигнорен (local-docs/), в
# репозиторий не едет: в git попадают только роли, адреса и ключи живут здесь.
# Значения подставить свои, лишние секции убрать.

[DEFAULT]
user = root
port = 22
key = ~/.ssh/rider

# Роль решает, что спрашивать: hub (юнит xr-hub и релиз приложения), server
# (юнит xr-proxy-server), router (процесс xr-client, правила nftables, exit-IP).
# units, binaries и nft_tables перекрывают набор роли целиком.

[hub-primary]
role = hub
host = 203.0.113.10
# Публичный адрес: по нему latest проверяется так, как его видит приложение.
url = https://hub.example.com
# Второй хаб живёт под тем же именем сайта, поэтому latest у обоих спрашивается
# ещё и изнутри машины, с Host-хедером.
host_header = hub.example.com

[hub-second]
role = hub
host = 198.51.100.20
host_header = hub.example.com
units = xr-hub, xr-web, xr-relay
binaries = /usr/local/bin/xr-hub, /usr/local/bin/xr-web, /usr/local/bin/xr-relay

[vps-de]
role = server
host = 192.0.2.30
# Выходной адрес этой VPS. Из таких значений собирается набор ожидаемых
# exit-IP для роутеров, у которых не задан свой expect_exit_ip.
exit_ip = 192.0.2.30

[router-ru]
role = router
host = 192.168.1.1
# Через какие VPS этому роутеру позволено выходить. Пусто значит «любой exit_ip
# из секций role = server».
expect_exit_ip = 192.0.2.30
nft_tables = xr_proxy, xr_udp_relay
"""


def fail(message):
    print(f"ОШИБКА: {message}", file=sys.stderr)
    sys.exit(2)


def split_list(raw):
    return [p.strip() for p in (raw or "").replace("\n", ",").split(",") if p.strip()]


class Machine:
    def __init__(self, name, section):
        self.name = name
        self.role = (section.get("role") or "").strip()
        if self.role not in ROLE_DEFAULTS:
            fail(
                f"секция [{name}]: роль {self.role or 'не задана'!r} неизвестна, "
                f"годятся {', '.join(sorted(ROLE_DEFAULTS))}"
            )
        self.host = (section.get("host") or "").strip()
        if not self.host:
            fail(f"секция [{name}]: нет host")
        self.user = (section.get("user") or "root").strip()
        self.port = (section.get("port") or "22").strip()
        self.key = os.path.expanduser((section.get("key") or "").strip())
        self.url = (section.get("url") or "").strip()
        self.host_header = (section.get("host_header") or "").strip()
        self.exit_ip = (section.get("exit_ip") or "").strip()
        self.expect_exit_ip = split_list(section.get("expect_exit_ip"))
        defaults = ROLE_DEFAULTS[self.role]
        self.units = split_list(section.get("units")) or list(defaults["units"])
        self.binaries = split_list(section.get("binaries")) or list(defaults["binaries"])
        self.nft_tables = split_list(section.get("nft_tables")) or list(defaults["nft_tables"])

    @property
    def target(self):
        return f"{self.user}@{self.host}:{self.port}"

    def ssh_argv(self, timeout):
        argv = [
            "ssh",
            "-p",
            self.port,
            "-o",
            "BatchMode=yes",
            "-o",
            f"ConnectTimeout={int(timeout)}",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]
        if self.key:
            argv += ["-i", self.key]
        return argv + [f"{self.user}@{self.host}"]


def read_config(text):
    parser = configparser.ConfigParser()
    parser.read_string(text)
    machines = [Machine(name, parser[name]) for name in parser.sections()]
    if not machines:
        fail("в конфиге нет ни одной машины")
    return machines


# Снимок машины. Один ssh на машину: собираем sh-скрипт, который печатает
# табулированные строки, и разбираем их здесь. Дробить на десяток ssh нельзя,
# опрос роутера за океаном и так самая долгая часть сводки.


def snapshot_script(machine):
    out = io.StringIO()
    out.write("set -u\n")
    out.write("printf 'uname\\t%s\\t%s\\n' \"$(uname -n)\" \"$(uname -r)\"\n")
    for path in machine.binaries:
        q = shlex.quote(path)
        out.write(
            f"if [ -f {q} ]; then\n"
            f"  md5=$(md5sum {q} 2>/dev/null | cut -d' ' -f1)\n"
            f"  sz=$(stat -c %s {q} 2>/dev/null || echo -)\n"
            f"  mt=$(stat -c %Y {q} 2>/dev/null || echo -)\n"
            f"  printf 'bin\\t%s\\tok\\t%s\\t%s\\t%s\\n' {q} \"${{md5:--}}\" \"$sz\" \"$mt\"\n"
            f"else\n"
            f"  printf 'bin\\t%s\\tmissing\\t-\\t-\\t-\\n' {q}\n"
            f"fi\n"
        )
    for unit in machine.units:
        q = shlex.quote(unit)
        out.write(
            f"st=$(systemctl is-active {q} 2>/dev/null || true)\n"
            f"since=$(systemctl show -p ActiveEnterTimestamp --value {q} 2>/dev/null || true)\n"
            f"nr=$(systemctl show -p NRestarts --value {q} 2>/dev/null || true)\n"
            f"printf 'unit\\t%s\\t%s\\t%s\\t%s\\n' {q} \"${{st:-unknown}}\" \"${{since:--}}\" \"${{nr:--}}\"\n"
        )
    for table in machine.nft_tables:
        q = shlex.quote(table)
        out.write(
            f"if nft list table ip {q} >/dev/null 2>&1; then\n"
            f"  printf 'nft\\t%s\\tok\\n' {q}\n"
            f"else\n"
            f"  printf 'nft\\t%s\\tmissing\\n' {q}\n"
            f"fi\n"
        )
    if machine.role == "router":
        out.write(
            "pid=$(pidof xr-client 2>/dev/null | tr ' ' '\\n' | head -1)\n"
            "if [ -n \"${pid:-}\" ]; then\n"
            "  rss=$(awk '/VmRSS/{print $2}' /proc/$pid/status 2>/dev/null)\n"
            "  printf 'proc\\txr-client\\tok\\t%s\\t%s\\n' \"$pid\" \"${rss:--}\"\n"
            "else\n"
            "  printf 'proc\\txr-client\\tmissing\\t-\\t-\\n'\n"
            "fi\n"
        )
        # Выходной адрес: HTTP 200 через прокси ничего не доказывает, Direct
        # отдаёт те же 200, поэтому судим по тому, чей адрес видит интернет.
        out.write(
            f"tr=$(curl -s --max-time 15 {TRACE_URL} 2>/dev/null "
            f"|| uclient-fetch -qO- --timeout=15 {TRACE_URL} 2>/dev/null || true)\n"
            "eip=$(printf '%s\\n' \"$tr\" | awk -F= '/^ip=/{print $2}')\n"
            "if [ -n \"${eip:-}\" ]; then\n"
            "  printf 'exit_ip\\tok\\t%s\\n' \"$eip\"\n"
            "else\n"
            "  printf 'exit_ip\\terror\\ttrace-no-answer\\n'\n"
            "fi\n"
        )
        out.write(
            "if [ -s /etc/xr-proxy/crash.log ]; then\n"
            "  printf 'crash\\tok\\t%s\\n' \"$(tail -1 /etc/xr-proxy/crash.log | cut -c1-160)\"\n"
            "fi\n"
        )
    if machine.role == "hub" and machine.host_header:
        out.write(
            f"resp=$(curl -sk --max-time 15 -H 'Host: {machine.host_header}' "
            "https://127.0.0.1/api/v1/app/latest 2>/dev/null || true)\n"
            "if [ -n \"${resp:-}\" ]; then\n"
            "  printf 'latest\\tok\\t%s\\n' \"$(printf '%s' \"$resp\" | tr -d '\\n\\t')\"\n"
            "else\n"
            "  printf 'latest\\terror\\tno-answer\\n'\n"
            "fi\n"
        )
    return out.getvalue()


def parse_snapshot(text):
    """Табулированные строки снимка в структуру. Незнакомые строки не роняют
    разбор: на машине мог отработать чей-то profile с приветствием."""
    snap = {"bins": [], "units": [], "nft": [], "proc": [], "extra": []}
    for line in text.splitlines():
        parts = line.rstrip("\r").split("\t")
        kind = parts[0] if parts else ""
        if kind == "uname" and len(parts) >= 3:
            snap["hostname"], snap["kernel"] = parts[1], parts[2]
        elif kind == "bin" and len(parts) >= 6:
            snap["bins"].append(
                {
                    "path": parts[1],
                    "state": parts[2],
                    "md5": parts[3],
                    "size": parts[4],
                    "mtime": parts[5],
                }
            )
        elif kind == "unit" and len(parts) >= 5:
            snap["units"].append(
                {"unit": parts[1], "state": parts[2], "since": parts[3], "restarts": parts[4]}
            )
        elif kind == "nft" and len(parts) >= 3:
            snap["nft"].append({"table": parts[1], "state": parts[2]})
        elif kind == "proc" and len(parts) >= 5:
            snap["proc"].append(
                {"name": parts[1], "state": parts[2], "pid": parts[3], "rss": parts[4]}
            )
        elif kind == "exit_ip" and len(parts) >= 3:
            snap["exit_ip"] = {"state": parts[1], "value": parts[2]}
        elif kind == "crash" and len(parts) >= 3:
            snap["crash"] = parts[2]
        elif kind == "latest" and len(parts) >= 3:
            snap["latest_inside"] = {"state": parts[1], "value": parts[2]}
        elif line.strip():
            snap["extra"].append(line.strip())
    return snap


def ssh_reason(stderr, code):
    """Причина отказа ssh одной строкой: в сводке важно не «недоступна», а чем
    именно она недоступна, иначе к машине идут вслепую."""
    for line in (stderr or "").splitlines():
        clean = line.strip()
        if not clean or clean.startswith("Warning: Permanently added"):
            continue
        return clean
    return f"ssh вернул код {code} без объяснений"


def probe(machine, timeout):
    argv = machine.ssh_argv(timeout) + [snapshot_script(machine)]
    try:
        done = subprocess.run(
            argv, capture_output=True, text=True, timeout=timeout + 60, check=False
        )
    except subprocess.TimeoutExpired:
        return {"machine": machine, "ok": False, "reason": f"ssh не уложился в {timeout + 60} с"}
    except OSError as err:
        return {"machine": machine, "ok": False, "reason": f"не запустился ssh: {err}"}
    if done.returncode != 0:
        return {"machine": machine, "ok": False, "reason": ssh_reason(done.stderr, done.returncode)}
    return {"machine": machine, "ok": True, "snapshot": parse_snapshot(done.stdout)}


def fetch_public_latest(machine, timeout):
    url = machine.url.rstrip("/") + "/api/v1/app/latest"
    argv = ["curl", "-fsS", "--max-time", str(int(timeout)), url]
    try:
        done = subprocess.run(
            argv, capture_output=True, text=True, timeout=timeout + 15, check=False
        )
    except (subprocess.TimeoutExpired, OSError) as err:
        return {"state": "error", "value": f"{type(err).__name__}"}
    if done.returncode != 0 or not done.stdout.strip():
        first = (done.stderr or "").strip().splitlines()
        return {
            "state": "error",
            "value": first[0] if first else f"curl вернул код {done.returncode}",
        }
    return {"state": "ok", "value": done.stdout.strip()}


def manifest_version(raw):
    """Из ответа /api/v1/app/latest вынуть версию релиза. Манифест внутри ответа
    лежит эскейпнутой строкой, поэтому json разбирается дважды."""
    try:
        outer = json.loads(raw)
        manifest = outer["manifest"]
        if isinstance(manifest, str):
            manifest = json.loads(manifest)
        name = manifest.get("version_name")
        code = manifest.get("version_code")
    except (ValueError, KeyError, TypeError, AttributeError):
        return None
    if name is None and code is None:
        return None
    return {"version_name": name, "version_code": code}


def human_size(raw):
    try:
        size = int(raw)
    except (TypeError, ValueError):
        return raw or "-"
    return f"{size / 1048576:.1f} МиБ" if size >= 1048576 else f"{size} Б"


def human_time(raw):
    try:
        stamp = int(raw)
    except (TypeError, ValueError):
        return raw or "-"
    return datetime.datetime.fromtimestamp(stamp).strftime("%Y-%m-%d %H:%M:%S")


# Разбор снимков: проблемы называются по имени машины и предмета, чтобы сводку
# можно было читать с конца.


def expected_exit_ips(machine, machines):
    if machine.expect_exit_ip:
        return machine.expect_exit_ip
    return [m.exit_ip for m in machines if m.role == "server" and m.exit_ip]


def review(results, machines):
    problems = []
    for res in results:
        name = res["machine"].name
        if not res["ok"]:
            problems.append(f"{name}: машина не ответила ({res['reason']})")
            continue
        snap = res["snapshot"]
        for item in snap["bins"]:
            if item["state"] != "ok":
                problems.append(f"{name}: нет бинаря {item['path']}")
            elif item["md5"] in ("", "-"):
                problems.append(f"{name}: не посчитался md5 бинаря {item['path']}")
        for item in snap["units"]:
            if item["state"] != "active":
                problems.append(f"{name}: юнит {item['unit']} не active ({item['state']})")
        for item in snap["nft"]:
            if item["state"] != "ok":
                problems.append(f"{name}: нет таблицы nftables ip {item['table']}")
        for item in snap["proc"]:
            if item["state"] != "ok":
                problems.append(f"{name}: процесс {item['name']} не найден")
        eip = snap.get("exit_ip")
        if eip is not None:
            expected = expected_exit_ips(res["machine"], machines)
            if eip["state"] != "ok":
                problems.append(f"{name}: exit-IP не выяснен ({eip['value']})")
            elif expected and eip["value"] not in expected:
                problems.append(
                    f"{name}: exit-IP {eip['value']} не из ожидаемых ({', '.join(expected)})"
                )
        inside = snap.get("latest_inside")
        if inside is not None and inside["state"] != "ok":
            problems.append(f"{name}: хаб не отдал latest изнутри машины ({inside['value']})")
        elif inside is not None and manifest_version(inside["value"]) is None:
            problems.append(f"{name}: latest изнутри машины не разобрался как манифест")
        public = res.get("latest_public")
        if public is not None and public["state"] != "ok":
            problems.append(f"{name}: публичный latest не ответил ({public['value']})")

    problems += review_builds(results)
    problems += review_releases(results)
    return problems


def review_builds(results):
    """Расхождение сборки по имени бинаря. Ровно то, за чем сводку и звали
    после выката: залил на один хаб, забыл про второй."""
    problems = []
    seen = {}
    for res in results:
        if not res["ok"]:
            continue
        for item in res["snapshot"]["bins"]:
            if item["state"] != "ok" or item["md5"] in ("", "-"):
                continue
            seen.setdefault(os.path.basename(item["path"]), {}).setdefault(item["md5"], []).append(
                res["machine"].name
            )
    for binary, by_md5 in sorted(seen.items()):
        if len(by_md5) < 2:
            continue
        where = "; ".join(
            f"{md5} на {', '.join(names)}" for md5, names in sorted(by_md5.items())
        )
        problems.append(f"сборка {binary} расходится по машинам: {where}")
    return problems


def review_releases(results):
    """Релиз приложения у хабов обязан быть один: приложение ходит на общее имя
    сайта и попадает то на один хаб, то на другой."""
    problems = []
    codes = {}
    for res in results:
        if not res["ok"]:
            continue
        name = res["machine"].name
        for label, payload in (
            ("изнутри машины", res["snapshot"].get("latest_inside")),
            ("по публичному адресу", res.get("latest_public")),
        ):
            if not payload or payload["state"] != "ok":
                continue
            version = manifest_version(payload["value"])
            if version is None:
                continue
            codes.setdefault(str(version["version_code"]), []).append(f"{name} ({label})")
    if len(codes) > 1:
        where = "; ".join(f"code {code} у {', '.join(who)}" for code, who in sorted(codes.items()))
        problems.append(f"хабы отдают разный релиз приложения: {where}")
    return problems


# Печать.


def print_report(results, machines, problems):
    for res in results:
        machine = res["machine"]
        print(f"=== {machine.name} ({machine.role}, {machine.target}) ===")
        if not res["ok"]:
            print(f"  НЕДОСТУПНА: {res['reason']}")
            continue
        snap = res["snapshot"]
        if snap.get("hostname"):
            print(f"  машина: {snap['hostname']}, ядро {snap.get('kernel', '-')}")
        for item in snap["bins"]:
            if item["state"] == "ok":
                print(
                    f"  бинарь {item['path']}: md5 {item['md5']}, "
                    f"{human_size(item['size'])}, {human_time(item['mtime'])}"
                )
            else:
                print(f"  бинарь {item['path']}: НЕТ ФАЙЛА")
        for item in snap["units"]:
            print(
                f"  юнит {item['unit']}: {item['state']}, с {item['since']}, "
                f"перезапусков {item['restarts']}"
            )
        for item in snap["proc"]:
            if item["state"] == "ok":
                rss = f"{item['rss']} КиБ" if item["rss"] not in ("", "-") else "неизвестен"
                print(f"  процесс {item['name']}: pid {item['pid']}, RSS {rss}")
            else:
                print(f"  процесс {item['name']}: НЕ НАЙДЕН")
        for item in snap["nft"]:
            print(
                f"  таблица nftables ip {item['table']}: "
                f"{'есть' if item['state'] == 'ok' else 'НЕТ'}"
            )
        eip = snap.get("exit_ip")
        if eip is not None:
            if eip["state"] == "ok":
                expected = expected_exit_ips(machine, machines)
                mark = ""
                if expected:
                    mark = " (ожидаемый)" if eip["value"] in expected else " (НЕ ОЖИДАЛСЯ)"
                print(f"  exit-IP: {eip['value']}{mark}")
            else:
                print(f"  exit-IP: НЕ ВЫЯСНЕН ({eip['value']})")
        for label, payload in (
            ("latest изнутри машины", snap.get("latest_inside")),
            (f"latest по адресу {machine.url}", res.get("latest_public")),
        ):
            if not payload:
                continue
            if payload["state"] != "ok":
                print(f"  {label}: НЕ ОТВЕТИЛ ({payload['value']})")
                continue
            version = manifest_version(payload["value"])
            if version is None:
                print(f"  {label}: ответ не разобрался как манифест")
            else:
                print(f"  {label}: {version['version_name']} (code {version['version_code']})")
        if snap.get("crash"):
            print(f"  последняя запись crash.log: {snap['crash']}")
        for line in snap["extra"]:
            print(f"  посторонний вывод: {line}")

    if problems:
        print()
        print("Проблемы:")
        for problem in problems:
            print(f"  {problem}")
    print()
    print("fleet status: ok" if not problems else f"fleet status: проблемы ({len(problems)})")


def report_json(results, problems):
    machines = []
    for res in results:
        row = {"name": res["machine"].name, "role": res["machine"].role, "reachable": res["ok"]}
        if res["ok"]:
            row.update(res["snapshot"])
            if res.get("latest_public") is not None:
                row["latest_public"] = res["latest_public"]
        else:
            row["reason"] = res["reason"]
        machines.append(row)
    return json.dumps({"machines": machines, "problems": problems}, ensure_ascii=False, indent=2)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--conf", help=f"конфиг флота (по умолчанию {DEFAULT_CONF} или FLEET_CONF)")
    parser.add_argument("--only", help="опросить только эти машины, через запятую")
    parser.add_argument("--timeout", type=float, default=20.0, help="таймаут подключения ssh")
    parser.add_argument("--json", action="store_true", help="печатать сводку машинно")
    args = parser.parse_args()

    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    conf_path = args.conf or os.environ.get("FLEET_CONF") or os.path.join(root, DEFAULT_CONF)
    if not os.path.isfile(conf_path):
        print(f"ОШИБКА: нет {conf_path}. Завести его по шаблону:", file=sys.stderr)
        print(CONF_TEMPLATE, file=sys.stderr)
        return 2
    with open(conf_path, encoding="utf-8") as handle:
        machines = read_config(handle.read())

    if args.only:
        wanted = split_list(args.only)
        unknown = [name for name in wanted if name not in [m.name for m in machines]]
        if unknown:
            fail(f"в конфиге нет машин: {', '.join(unknown)}")
        machines = [m for m in machines if m.name in wanted]

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(4, len(machines))) as pool:
        probes = {pool.submit(probe, m, args.timeout): m for m in machines}
        by_name = {}
        for future in concurrent.futures.as_completed(probes):
            res = future.result()
            by_name[res["machine"].name] = res
        publics = {
            pool.submit(fetch_public_latest, m, args.timeout): m
            for m in machines
            if m.role == "hub" and m.url
        }
        for future in concurrent.futures.as_completed(publics):
            by_name[publics[future].name]["latest_public"] = future.result()

    results = [by_name[m.name] for m in machines]
    problems = review(results, machines)
    if args.json:
        print(report_json(results, problems))
    else:
        print_report(results, machines, problems)
    return 0 if not problems else 1


if __name__ == "__main__":
    sys.exit(main())
