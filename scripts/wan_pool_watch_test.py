#!/usr/bin/env python3
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class WatchTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.state = self.root / "state"
        self.sysfs = self.root / "sys"
        for dev in ("wan", "lan1"):
            (self.sysfs / dev).mkdir(parents=True)
            (self.sysfs / dev / "carrier").write_text("1\n")
        self.config = self.root / "pool.conf"
        self.config.write_text('MEMBERS="wan:1 lan1:3"\nBYPASS_IPS="192.0.2.10"\n')
        self.env = dict(os.environ, PATH=str(self.bin) + ":" + os.environ["PATH"],
                        XR_WAN_POOL_CONFIG=str(self.config), XR_WAN_POOL_STATE=str(self.state),
                        XR_WAN_POOL_SYSFS=str(self.sysfs), TEST_ROOT=str(self.root))
        self.script("ping", 'while test "$#" -gt 0; do if test "$1" = -I; then dev=$2; break; fi; shift; done\ntest ! -e "$TEST_ROOT/fail.$dev"\n')
        self.script("ip", 'echo "default via 192.0.2.1 dev wan"\n')
        self.script("logger", "exit 0\n")
        self.script("nft", "exit 0\n")
        setup = self.script("setup", 'printf "%s\\n" "$*" >> "$TEST_ROOT/applies"\n')
        self.env["XR_WAN_POOL_SETUP"] = str(setup)
        self.kernel_model()

    def tearDown(self):
        self.tmp.cleanup()

    def script(self, name, body):
        p = self.bin / name
        p.write_text("#!/bin/sh\n" + body)
        p.chmod(0o755)
        return p

    def run_once(self):
        watcher = Path(__file__).with_name("wan-pool-watch.sh")
        subprocess.run(["sh", str(watcher), "--once"], env=self.env, check=True,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)

    def last_apply(self):
        return (self.root / "applies").read_text().splitlines()[-1]

    def apply_count(self):
        return len((self.root / "applies").read_text().splitlines())

    def kernel_model(self):
        self.routes = self.root / "routes"
        self.rules = self.root / "rules"
        self.routes.write_text("")
        self.rules.write_text("")
        (self.root / "expected-routes").write_text(
            "default via 192.0.2.1 dev wan table 210 metric 10 onlink\n"
            "unreachable default table 210 metric 32767\n"
            "default via 198.51.100.1 dev lan1 table 211 metric 10 onlink\n"
            "unreachable default table 211 metric 32767\n"
            "unreachable default table 218 metric 32767\n"
            "default via 192.0.2.1 dev wan table 218 metric 10 onlink\n")
        (self.root / "expected-rules").write_text(
            "11000: from all fwmark 0x1000000/0xf000000 lookup 210\n"
            "11100: from all oif wan lookup 210\n"
            "11001: from all fwmark 0x2000000/0xf000000 lookup 211\n"
            "11101: from all oif lan1 lookup 211\n"
            "32763: not from all fwmark 0x200/0x200 lookup main suppress_prefixlength 0\n"
            "32764: not from all fwmark 0x200/0x200 lookup 218\n")
        self.script("ip", '''case "$*" in
    "-4 route show table all")
        echo "default via 192.0.2.1 dev wan"
        cat "$TEST_ROOT/routes" ;;
    "-4 route show table all default dev "*) echo "default via 192.0.2.1 dev wan" ;;
    "-4 rule show") cat "$TEST_ROOT/rules" ;;
    "link show dev "*)
        dev=$4
        if test "$(cat "$TEST_ROOT/sys/$dev/carrier")" = 1; then echo "2: $dev: <UP,LOWER_UP>"; fi ;;
    *) echo "2: wan inet 192.0.2.2/24 scope global" ;;
esac
''')
        self.script("setup", '''printf "%s\\n" "$*" >> "$TEST_ROOT/applies"
cp "$TEST_ROOT/expected-routes" "$TEST_ROOT/routes"
cp "$TEST_ROOT/expected-rules" "$TEST_ROOT/rules"
if test -e "$TEST_ROOT/delete-after-setup"; then
    table=$(cat "$TEST_ROOT/delete-after-setup")
    sed "/^default .* table $table /d" "$TEST_ROOT/routes" > "$TEST_ROOT/trimmed"
    mv "$TEST_ROOT/trimmed" "$TEST_ROOT/routes"
fi
''')
        self.script("ping", '''while test "$#" -gt 0; do
    if test "$1" = -I; then dev=$2; break; fi
    shift
done
test ! -e "$TEST_ROOT/fail.$dev" || exit 1
if test "$dev" = wan; then
    grep -q '^default .* table 210 ' "$TEST_ROOT/routes"
fi
''')

    def test_missing_owned_probe_default_is_repaired_before_health_pass(self):
        self.kernel_model()
        self.run_once()
        (self.root / "fail.wan").touch()
        for _ in range(3):
            self.run_once()
        self.assertIn("--down wan", self.last_apply())
        self.assertEqual((self.state / "current").read_text(),
                         (self.state / "applied").read_text())
        applies = self.apply_count()
        before = self.routes.read_text().splitlines(keepends=True)
        self.routes.write_text("".join(line for line in before
                                       if not line.startswith("default via 192.0.2.1 dev wan table 210")))
        (self.root / "fail.wan").unlink()
        self.run_once()
        self.assertEqual(self.apply_count(), applies + 1)
        self.assertIn("default via 192.0.2.1 dev wan table 210", self.routes.read_text())
        self.assertEqual((self.state / "pass.wan").read_text(), "1\n")
        self.assertIn("--down wan", self.last_apply())
        self.run_once()
        self.assertEqual(self.apply_count(), applies + 1)

    def test_missing_owned_rule_is_repaired_without_refresh_loop(self):
        self.kernel_model()
        self.run_once()
        self.rules.write_text("".join(line for line in self.rules.read_text().splitlines(keepends=True)
                                      if not line.startswith("11100:")))
        self.run_once()
        self.assertEqual(self.apply_count(), 2)
        self.assertIn("11100: from all oif wan lookup 210", self.rules.read_text())
        self.run_once()
        self.assertEqual(self.apply_count(), 2)

    def test_foreign_kernel_resources_do_not_trigger_refresh(self):
        self.kernel_model()
        self.run_once()
        with self.routes.open("a") as routes:
            routes.write("local default dev lo table 201\n"
                         "default via 203.0.113.1 dev foreign table 212\n")
        with self.rules.open("a") as rules:
            rules.write("9998: from all fwmark 0x10000000 lookup 218\n"
                        "9997: from all fwmark 0x200 lookup 201\n"
                        "11002: from all fwmark 0x3000000/0xf000000 lookup 212\n"
                        "11102: from all oif foreign lookup 212\n")
        self.run_once()
        self.assertEqual(self.apply_count(), 1)

    def test_route_deleted_between_setup_and_snapshot_is_not_accepted(self):
        self.assert_post_apply_route_deletion_retried(210)

    def test_fallback_deleted_between_setup_and_snapshot_is_not_accepted(self):
        self.assert_post_apply_route_deletion_retried(218)

    def assert_post_apply_route_deletion_retried(self, table):
        self.run_once()
        baseline = (self.state / "kernel.applied").read_text()
        self.rules.write_text("")
        (self.root / "delete-after-setup").write_text(str(table))
        self.run_once()
        self.assertEqual(self.apply_count(), 2)
        self.assertEqual((self.state / "kernel.applied").read_text(), baseline)
        self.assertNotIn(f"default via 192.0.2.1 dev wan table {table}", self.routes.read_text())
        (self.root / "delete-after-setup").unlink()
        self.run_once()
        self.assertEqual(self.apply_count(), 3)
        self.assertIn(f"default via 192.0.2.1 dev wan table {table}", self.routes.read_text())
        self.run_once()
        self.assertEqual(self.apply_count(), 3)

    def test_incomplete_fallback_rule_is_not_accepted(self):
        expected = self.root / "expected-rules"
        rules = expected.read_text()
        expected.write_text(rules.replace(" suppress_prefixlength 0", ""))
        self.run_once()
        self.assertFalse((self.state / "kernel.applied").exists())
        expected.write_text(rules)
        self.run_once()
        self.assertTrue((self.state / "kernel.applied").exists())
        self.run_once()
        self.assertEqual(self.apply_count(), 2)

    def test_iproute2_tab_separated_rules_are_accepted(self):
        expected = self.root / "expected-rules"
        expected.write_text(expected.read_text().replace(": ", ":\t"))
        self.run_once()
        self.assertTrue((self.state / "kernel.applied").exists())
        self.run_once()
        self.assertEqual(self.apply_count(), 1)

    def test_busy_lock_does_not_accept_kernel_drift(self):
        self.kernel_model()
        self.run_once()
        baseline = (self.state / "kernel.applied").read_text()
        self.rules.write_text("")
        (self.state / "lock").mkdir()
        self.run_once()
        self.assertEqual(self.apply_count(), 1)
        self.assertEqual((self.state / "kernel.applied").read_text(), baseline)
        (self.state / "lock").rmdir()
        self.run_once()
        self.assertEqual(self.apply_count(), 2)
        self.assertEqual(self.rules.read_text(), (self.root / "expected-rules").read_text())

    def test_live_carrier_internet_failure_and_hysteresis(self):
        self.run_once()
        self.assertNotIn("--down", self.last_apply())
        (self.root / "fail.lan1").touch()
        self.run_once()
        self.run_once()
        self.assertNotIn("--down", self.last_apply())
        self.run_once()
        self.assertIn("--down lan1", self.last_apply())
        (self.root / "fail.lan1").unlink()
        self.run_once()
        self.run_once()
        self.assertIn("--down lan1", self.last_apply())
        self.run_once()
        self.assertNotIn("--down", self.last_apply())
        self.assertIn("--bypass 192.0.2.10", self.last_apply())

    def test_carrier_loss_is_immediate(self):
        (self.sysfs / "lan1" / "carrier").write_text("0\n")
        self.run_once()
        self.assertIn("--down lan1", self.last_apply())

    def test_owned_router_fallback_route_does_not_trigger_refresh_loop(self):
        (self.bin / "ip").rename(self.bin / "ip-base")
        self.script("ip", """if test "$2" = route && test "$#" -gt 5; then
    n=$(cat "$TEST_ROOT/route-counter" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$TEST_ROOT/route-counter"
    echo "default via 192.0.2.1 dev wan"
    echo "default via 192.0.2.$n dev wan table 218"
else
    exec "$TEST_ROOT/bin/ip-base" "$@"
fi
""")
        self.run_once()
        self.run_once()
        self.assertEqual(len((self.root / "applies").read_text().splitlines()), 1)

    def test_failed_apply_is_retried(self):
        self.script("setup", "exit 1\n")
        self.run_once()
        self.assertFalse((self.state / "applied").exists())
        self.kernel_model()
        self.run_once()
        self.assertTrue((self.state / "applied").exists())


if __name__ == "__main__":
    unittest.main()
