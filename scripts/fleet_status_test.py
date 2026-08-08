#!/usr/bin/env python3
"""Прогон scripts/fleet-status.py на стенде из заглушек (XR-113).

Настоящие ssh и curl звать нельзя, а флот стенда это каталог на машину. Поэтому
судится не текст скрипта, а сводка после прогона: заглушка ssh запускает
собранный скрипт снимка настоящим sh, а заглушки systemctl, nft, pidof, curl и
uname отвечают из каталога своей машины. md5 и размер считаются по-настоящему,
файлы бинарей на стенде лежат.

Отдельно проверяется наблюдаемость: выключенная машина обязана попасть в сводку
строкой с причиной и уронить код возврата, а не молча выпасть из таблицы.

  python3 scripts/fleet_status_test.py
"""

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "fleet-status.py")

spec = importlib.util.spec_from_file_location("fleet_status", SCRIPT)
fs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fs)

STUB_SSH = r'''#!/usr/bin/env python3
import os, subprocess, sys
stand = os.environ["STAND"]
args = sys.argv[1:]
with open(os.path.join(stand, "calls.log"), "a") as f:
    f.write("ssh " + " ".join(args[:-1]) + "\n")
snippet = args[-1]
target = [a for a in args[:-1] if "@" in a][-1]
host = target.split("@", 1)[1]
hostdir = os.path.join(stand, "hosts", host)
down = os.path.join(hostdir, "down")
if os.path.exists(down):
    sys.stderr.write(open(down).read())
    sys.exit(255)
env = dict(os.environ)
env["HOSTDIR"] = hostdir
env["PATH"] = os.path.join(stand, "bin") + ":/usr/bin:/bin:/usr/sbin:/sbin"
sys.exit(subprocess.run(["sh", "-c", snippet], env=env).returncode)
'''

# Заглушки coreutils: на макбуке нет ни md5sum, ни GNU-шного stat -c, а флот
# весь на Linux. Хеш и размер считаются настоящие, подмены значений нет.
STUB_MD5SUM = r'''#!/usr/bin/env python3
import hashlib, sys
path = sys.argv[1]
try:
    data = open(path, "rb").read()
except OSError:
    sys.stderr.write("md5sum: no such file\n")
    sys.exit(1)
print(hashlib.md5(data).hexdigest() + "  " + path)
'''

STUB_STAT = r'''#!/usr/bin/env python3
import os, sys
fmt, path = sys.argv[2], sys.argv[3]
try:
    st = os.stat(path)
except OSError:
    sys.exit(1)
print(int(st.st_size) if fmt == "%s" else int(st.st_mtime))
'''

STUB_UNAME = r'''#!/usr/bin/env python3
import os, sys
lines = open(os.path.join(os.environ["HOSTDIR"], "uname")).read().splitlines()
print(lines[0] if sys.argv[1] == "-n" else lines[1])
'''

# Юнит машины лежит файлом hosts/<host>/units/<имя>: состояние, метка запуска,
# число перезапусков.
STUB_SYSTEMCTL = r'''#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
unit = args[-1]
path = os.path.join(os.environ["HOSTDIR"], "units", unit)
if not os.path.exists(path):
    sys.stderr.write("Unit not found\n")
    sys.exit(4)
state, since, restarts = (open(path).read().splitlines() + ["", "", ""])[:3]
if args[0] == "is-active":
    print(state)
    sys.exit(0 if state == "active" else 3)
if args[0] == "show":
    print(since if "ActiveEnterTimestamp" in " ".join(args) else restarts)
    sys.exit(0)
sys.exit(1)
'''

STUB_NFT = r'''#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
if args[:3] != ["list", "table", "ip"]:
    sys.exit(1)
sys.exit(0 if os.path.exists(os.path.join(os.environ["HOSTDIR"], "nft", args[3])) else 1)
'''

STUB_PIDOF = r'''#!/usr/bin/env python3
import os, sys
path = os.path.join(os.environ["HOSTDIR"], "pid")
if not os.path.exists(path):
    sys.exit(1)
print(open(path).read().strip())
'''

# curl стенда: трассировка и latest хаба. Изнутри машины отвечает её каталог,
# по публичному адресу общий файл стенда.
STUB_CURL = r'''#!/usr/bin/env python3
import os, sys
stand = os.environ["STAND"]
args = sys.argv[1:]
with open(os.path.join(stand, "calls.log"), "a") as f:
    f.write("curl " + " ".join(args) + "\n")
urls = [a for a in args if a.startswith("http")]
if not urls:
    sys.exit(2)
url = urls[-1]
hostdir = os.environ.get("HOSTDIR", "")
if "cdn-cgi/trace" in url:
    path = os.path.join(hostdir, "exit_ip")
    if not os.path.exists(path):
        sys.exit(7)
    sys.stdout.write("fl=1f1\nip=" + open(path).read().strip() + "\nts=1\n")
    sys.exit(0)
if "/app/latest" in url:
    path = os.path.join(hostdir, "latest") if hostdir else os.path.join(stand, "public_latest")
    if not os.path.exists(path):
        sys.stderr.write("curl: (22) HTTP 404\n")
        sys.exit(22)
    sys.stdout.write(open(path).read())
    sys.exit(0)
sys.exit(2)
'''

STUBS = {
    "ssh": STUB_SSH,
    "curl": STUB_CURL,
    "md5sum": STUB_MD5SUM,
    "stat": STUB_STAT,
    "uname": STUB_UNAME,
    "systemctl": STUB_SYSTEMCTL,
    "nft": STUB_NFT,
    "pidof": STUB_PIDOF,
}


def manifest(code, name):
    inner = json.dumps({"version_name": name, "version_code": code})
    return json.dumps({"manifest": inner, "signature": "c2ln"})


class Stand:
    """Флот из заглушек: два хаба, VPS и роутер. Каждый тест правит стенд под
    свою мутацию и гоняет скрипт целиком."""

    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix="fleet-stand-")
        self.bin = os.path.join(self.dir, "bin")
        os.makedirs(self.bin)
        for name, body in STUBS.items():
            path = os.path.join(self.bin, name)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(body)
            os.chmod(path, 0o755)
        self.binaries = os.path.join(self.dir, "binaries")
        os.makedirs(self.binaries)
        self.hub_bin = self.binary("xr-hub", b"hub build one")
        self.server_bin = self.binary("xr-server", b"server build one")
        self.client_bin = self.binary("xr-client", b"client build one")
        for host in ("hub1", "hub2", "vps1", "router1"):
            self.host_dir(host)
        self.unit("hub1", "xr-hub", "active", "Fri 2026-08-08 10:11:20 UTC", "0")
        self.unit("hub2", "xr-hub", "active", "Fri 2026-08-08 10:12:30 UTC", "0")
        self.unit("vps1", "xr-proxy-server", "active", "Fri 2026-08-08 09:00:00 UTC", "1")
        self.write("hosts/hub1/latest", manifest(42, "0.42.0"))
        self.write("hosts/hub2/latest", manifest(42, "0.42.0"))
        self.write("public_latest", manifest(42, "0.42.0"))
        self.write("hosts/router1/pid", "4242")
        self.write("hosts/router1/exit_ip", "192.0.2.30")
        self.write("hosts/router1/nft/xr_proxy", "")
        self.conf_path = os.path.join(self.dir, "fleet.ini")
        self.write_conf()

    # --- стенд ---

    def host_dir(self, host):
        path = os.path.join(self.dir, "hosts", host)
        os.makedirs(path, exist_ok=True)
        self.write(f"hosts/{host}/uname", f"{host}.local\n5.10.0-stand\n")
        return path

    def write(self, rel, body):
        path = os.path.join(self.dir, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(body)
        return path

    def binary(self, name, body, where=""):
        base = os.path.join(self.binaries, where) if where else self.binaries
        os.makedirs(base, exist_ok=True)
        path = os.path.join(base, name)
        with open(path, "wb") as handle:
            handle.write(body)
        return path

    def unit(self, host, unit, state, since, restarts):
        self.write(f"hosts/{host}/units/{unit}", f"{state}\n{since}\n{restarts}\n")

    def md5(self, path):
        with open(path, "rb") as handle:
            return hashlib.md5(handle.read()).hexdigest()

    def write_conf(self, **over):
        hub2_bin = over.get("hub2_bin", self.hub_bin)
        router_expect = over.get("router_expect", "192.0.2.30")
        conf = f"""
[DEFAULT]
user = root
port = 22
key = {os.path.join(self.dir, 'fake.key')}

[hub-primary]
role = hub
host = hub1
url = http://hub.public.test
host_header = hub.public.test
binaries = {self.hub_bin}

[hub-second]
role = hub
host = hub2
host_header = hub.public.test
binaries = {hub2_bin}

[vps-de]
role = server
host = vps1
port = 2222
exit_ip = 192.0.2.30
binaries = {self.server_bin}

[router-ru]
role = router
host = router1
expect_exit_ip = {router_expect}
binaries = {self.client_bin}
"""
        with open(self.conf_path, "w", encoding="utf-8") as handle:
            handle.write(conf)

    def calls(self):
        path = os.path.join(self.dir, "calls.log")
        if not os.path.exists(path):
            return ""
        with open(path, encoding="utf-8") as handle:
            return handle.read()

    def run(self, *args, conf=True):
        env = dict(os.environ)
        env["STAND"] = self.dir
        env["PATH"] = self.bin + ":" + env.get("PATH", "")
        if conf:
            env["FLEET_CONF"] = self.conf_path
        else:
            env.pop("FLEET_CONF", None)
            env["FLEET_CONF"] = os.path.join(self.dir, "нет-такого.ini")
        return subprocess.run(
            [sys.executable, SCRIPT, *args],
            capture_output=True,
            text=True,
            env=env,
            timeout=180,
            check=False,
        )

    def cleanup(self):
        shutil.rmtree(self.dir, ignore_errors=True)


class FleetStatusStand(unittest.TestCase):
    def setUp(self):
        self.stand = Stand()
        self.addCleanup(self.stand.cleanup)

    def test_template_without_conf(self):
        done = self.stand.run(conf=False)
        self.assertEqual(done.returncode, 2, done.stderr)
        self.assertIn("Завести его по шаблону", done.stderr)
        self.assertIn("role = router", done.stderr)
        # Шаблон обязан разбираться самим скриптом, иначе владелец заведёт по
        # нему конфиг и получит отказ парсера.
        machines = fs.read_config(fs.CONF_TEMPLATE)
        self.assertEqual(
            [(m.name, m.role) for m in machines],
            [
                ("hub-primary", "hub"),
                ("hub-second", "hub"),
                ("vps-de", "server"),
                ("router-ru", "router"),
            ],
        )

    def test_green_fleet(self):
        done = self.stand.run()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        out = done.stdout
        self.assertIn("fleet status: ok", out)
        self.assertNotIn("Проблемы:", out)
        self.assertIn(f"md5 {self.stand.md5(self.stand.hub_bin)}", out)
        self.assertIn(f"md5 {self.stand.md5(self.stand.client_bin)}", out)
        self.assertIn("юнит xr-hub: active", out)
        self.assertIn("юнит xr-proxy-server: active", out)
        self.assertIn("процесс xr-client: pid 4242", out)
        self.assertIn("таблица nftables ip xr_proxy: есть", out)
        self.assertIn("exit-IP: 192.0.2.30 (ожидаемый)", out)
        self.assertIn("latest изнутри машины: 0.42.0 (code 42)", out)
        self.assertIn("latest по адресу http://hub.public.test: 0.42.0 (code 42)", out)
        self.assertIn("машина: hub1.local, ядро 5.10.0-stand", out)
        # Порт и ключ из конфига обязаны доехать до ssh, иначе сводка молча
        # опрашивает не ту машину.
        self.assertIn("-p 2222", self.stand.calls())
        self.assertIn("-i " + os.path.join(self.stand.dir, "fake.key"), self.stand.calls())

    def test_unreachable_machine_is_named(self):
        self.stand.write("hosts/vps1/down", "ssh: connect to host vps1 port 2222: Timed out\n")
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("НЕДОСТУПНА: ssh: connect to host vps1 port 2222: Timed out", done.stdout)
        self.assertIn("vps-de: машина не ответила", done.stdout)
        # Остальной флот опрашивается дальше, одна дохлая машина не срывает сводку.
        self.assertIn("юнит xr-hub: active", done.stdout)
        self.assertIn("exit-IP: 192.0.2.30", done.stdout)

    def test_build_divergence_between_hubs(self):
        # Имя бинаря на машинах одно, а содержимое разное: ровно то, что даёт
        # выкат на один хаб из двух.
        other = self.stand.binary("xr-hub", b"hub build two", where="second")
        self.stand.write_conf(hub2_bin=other)
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("сборка xr-hub расходится по машинам", done.stdout)
        self.assertIn(self.stand.md5(other), done.stdout)

    def test_release_divergence_between_hubs(self):
        self.stand.write("hosts/hub2/latest", manifest(41, "0.41.0"))
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("хабы отдают разный релиз приложения", done.stdout)
        self.assertIn("code 41", done.stdout)
        self.assertIn("code 42", done.stdout)

    def test_hub_without_release_is_a_problem(self):
        os.remove(os.path.join(self.stand.dir, "hosts/hub2/latest"))
        os.remove(os.path.join(self.stand.dir, "public_latest"))
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("hub-second: хаб не отдал latest изнутри машины", done.stdout)
        self.assertIn("hub-primary: публичный latest не ответил", done.stdout)

    def test_dead_unit_is_named(self):
        self.stand.unit("hub2", "xr-hub", "failed", "-", "7")
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("hub-second: юнит xr-hub не active (failed)", done.stdout)

    def test_missing_binary_and_rules_and_process(self):
        os.remove(self.stand.client_bin)
        shutil.rmtree(os.path.join(self.stand.dir, "hosts/router1/nft"))
        os.remove(os.path.join(self.stand.dir, "hosts/router1/pid"))
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn(f"router-ru: нет бинаря {self.stand.client_bin}", done.stdout)
        self.assertIn("router-ru: нет таблицы nftables ip xr_proxy", done.stdout)
        self.assertIn("router-ru: процесс xr-client не найден", done.stdout)
        self.assertIn("НЕ НАЙДЕН", done.stdout)

    def test_exit_ip_not_expected(self):
        self.stand.write("hosts/router1/exit_ip", "198.51.100.77")
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn(
            "router-ru: exit-IP 198.51.100.77 не из ожидаемых (192.0.2.30)", done.stdout
        )
        self.assertIn("(НЕ ОЖИДАЛСЯ)", done.stdout)

    def test_exit_ip_unknown(self):
        os.remove(os.path.join(self.stand.dir, "hosts/router1/exit_ip"))
        done = self.stand.run()
        self.assertEqual(done.returncode, 1, done.stdout)
        self.assertIn("router-ru: exit-IP не выяснен (trace-no-answer)", done.stdout)

    def test_crash_log_tail_shown(self):
        # Роутер стенда ходит в /etc/xr-proxy/crash.log настоящей машины, там
        # файла нет; проверяем, что отсутствие журнала не ломает снимок.
        done = self.stand.run("--only", "router-ru")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertNotIn("crash.log", done.stdout)
        self.assertEqual(self.stand.calls().count("ssh "), 1)

    def test_json_report(self):
        self.stand.write("hosts/vps1/down", "ssh: Connection refused\n")
        done = self.stand.run("--json")
        self.assertEqual(done.returncode, 1, done.stdout)
        report = json.loads(done.stdout)
        rows = {row["name"]: row for row in report["machines"]}
        self.assertEqual(
            sorted(rows), ["hub-primary", "hub-second", "router-ru", "vps-de"]
        )
        self.assertFalse(rows["vps-de"]["reachable"])
        self.assertIn("Connection refused", rows["vps-de"]["reason"])
        self.assertEqual(rows["router-ru"]["exit_ip"], {"state": "ok", "value": "192.0.2.30"})
        self.assertEqual(
            rows["hub-primary"]["bins"][0]["md5"], self.stand.md5(self.stand.hub_bin)
        )
        self.assertTrue(any("vps-de" in p for p in report["problems"]))

    def test_unknown_machine_in_only(self):
        done = self.stand.run("--only", "router-ru,нет-такой")
        self.assertEqual(done.returncode, 2, done.stdout)
        self.assertIn("в конфиге нет машин: нет-такой", done.stderr)


class PureFunctions(unittest.TestCase):
    def test_manifest_version(self):
        self.assertEqual(
            fs.manifest_version(manifest(42, "0.42.0")),
            {"version_name": "0.42.0", "version_code": 42},
        )
        # Манифест иногда приходит уже разобранным объектом, а не строкой.
        raw = json.dumps({"manifest": {"version_name": "1.0.0", "version_code": 7}})
        self.assertEqual(
            fs.manifest_version(raw), {"version_name": "1.0.0", "version_code": 7}
        )
        self.assertIsNone(fs.manifest_version("не json"))
        self.assertIsNone(fs.manifest_version(json.dumps({"signature": "c2ln"})))
        self.assertIsNone(fs.manifest_version(json.dumps({"manifest": "{}"})))

    def test_snapshot_parsing_survives_alien_output(self):
        snap = fs.parse_snapshot(
            "Welcome to Ubuntu\n"
            "uname\tvps1\t5.10.0\n"
            "bin\t/usr/local/bin/xr-server\tok\tabc\t100\t1754640000\n"
            "unit\txr-proxy-server\tactive\tFri 2026-08-08\t0\n"
        )
        self.assertEqual(snap["hostname"], "vps1")
        self.assertEqual(snap["bins"][0]["md5"], "abc")
        self.assertEqual(snap["units"][0]["state"], "active")
        self.assertEqual(snap["extra"], ["Welcome to Ubuntu"])

    def test_expected_exit_ips_falls_back_to_servers(self):
        machines = fs.read_config(
            """
[vps-a]
role = server
host = 1.1.1.1
exit_ip = 1.1.1.1

[vps-b]
role = server
host = 2.2.2.2
exit_ip = 2.2.2.2

[router-ru]
role = router
host = 192.168.1.1
"""
        )
        router = machines[-1]
        self.assertEqual(fs.expected_exit_ips(router, machines), ["1.1.1.1", "2.2.2.2"])
        router.expect_exit_ip = ["2.2.2.2"]
        self.assertEqual(fs.expected_exit_ips(router, machines), ["2.2.2.2"])

    def test_role_defaults(self):
        machines = fs.read_config(
            "[r]\nrole = router\nhost = h\n\n[s]\nrole = server\nhost = h2\n"
        )
        router, server = machines
        self.assertEqual(router.binaries, ["/usr/bin/xr-client"])
        self.assertEqual(router.nft_tables, ["xr_proxy"])
        self.assertEqual(server.units, ["xr-proxy-server"])

    def test_ssh_reason_skips_host_key_noise(self):
        reason = fs.ssh_reason(
            "Warning: Permanently added 'vps1' to the list of known hosts.\n"
            "ssh: connect to host vps1 port 22: Operation timed out\n",
            255,
        )
        self.assertEqual(reason, "ssh: connect to host vps1 port 22: Operation timed out")


if __name__ == "__main__":
    unittest.main(verbosity=2)
