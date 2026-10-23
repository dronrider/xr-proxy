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
        self.script("ip", """if test "$2" = route; then
    n=$(cat "$TEST_ROOT/route-counter" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$TEST_ROOT/route-counter"
    echo "default via 192.0.2.1 dev wan"
    echo "default via 192.0.2.$n dev wan table 218"
else
    echo "2: wan inet 192.0.2.2/24 scope global"
fi
""")
        self.run_once()
        self.run_once()
        self.assertEqual(len((self.root / "applies").read_text().splitlines()), 1)

    def test_failed_apply_is_retried(self):
        self.script("setup", "exit 1\n")
        self.run_once()
        self.assertFalse((self.state / "applied").exists())
        self.script("setup", 'echo "$*" >> "$TEST_ROOT/applies"\n')
        self.run_once()
        self.assertTrue((self.state / "applied").exists())


if __name__ == "__main__":
    unittest.main()
