#!/usr/bin/env python3
"""Run the pool scripts against a stateful, isolated ip/nft command fixture."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent
STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
base = pathlib.Path(os.environ['FIXTURE'])
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with (base / 'calls').open('a') as out:
    out.write(json.dumps([name] + args) + '\n')
cfg = json.loads((base / 'fixture.json').read_text())
if name == 'nft':
    if args[:2] == ['list', 'table']:
        sys.exit(0 if (base / 'nft-owned').exists() else 1)
    if '-f' in args:
        source = pathlib.Path(args[args.index('-f') + 1]).read_text()
        (base / ('checked.nft' if '-c' in args else 'applied.nft')).write_text(source)
        if cfg.get('nft_fail'):
            sys.exit(1)
        if '-c' not in args:
            (base / 'nft-owned').touch()
    if args[:2] == ['delete', 'table']:
        (base / 'nft-owned').unlink(missing_ok=True)
elif args[:4] == ['-4', 'route', 'show', 'table']:
    table = args[4]
    print(cfg.get('routes' if table == 'all' else 'main' if table == 'main' else 'occupied', ''), end='')
elif args[:3] == ['-4', 'rule', 'show']:
    print((base / 'rules').read_text() if (base / 'rules').exists() else '', end='')
elif args[:3] == ['-4', 'rule', 'add']:
    with (base / 'rules').open('a') as out:
        out.write(args[4] + ': ' + ' '.join(args[5:]) + '\n')
elif args[:3] == ['link', 'show', 'dev']:
    print('2: ' + args[3] + ': <UP' + (',LOWER_UP' if args[3] not in cfg.get('carrier_down', []) else '') + '>')
elif args[:5] == ['-o', '-4', 'addr', 'show', 'dev']:
    if args[5] not in cfg.get('no_address', []):
        print('2: ' + args[5] + ' inet 192.0.2.4/24 scope global')
'''


class PoolScripts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="xr-wan-pool-test-")
        self.base = Path(self.tmp.name)
        self.bin = self.base / "bin"
        self.bin.mkdir()
        for command in ("ip", "nft"):
            script = self.bin / command
            script.write_text(STUB)
            script.chmod(0o755)
        self.cfg = {
            "routes": "default via 198.51.100.1 dev wan\n"
            "default via 192.168.0.1 dev lan1 table 202\n",
            "main": "default via 198.51.100.1 dev wan\n"
            "10.10.10.0/24 dev wg0\n203.0.113.0/24 dev wg0\n",
        }
        self.env = dict(os.environ, PATH=str(self.bin) + ":" + os.environ["PATH"],
                        FIXTURE=str(self.base), XR_WAN_POOL_STATE_DIR=str(self.base / "state"))

    def tearDown(self):
        self.tmp.cleanup()

    def run_script(self, extra=(), cleanup=False, members=None):
        (self.base / "fixture.json").write_text(json.dumps(self.cfg))
        args = [] if cleanup else (["--member", "wan:1", "--member", "lan1:3"] if members is None else members) + ["--bypass", "192.168.1.42"]
        return subprocess.run(["sh", str(ROOT / ("wan-pool-cleanup.sh" if cleanup else "wan-pool-setup.sh")),
                               *args, *extra], env=self.env, text=True, capture_output=True)

    def calls(self):
        return [json.loads(line) for line in (self.base / "calls").read_text().splitlines()]

    def test_weighted_pool_preserves_tproxy_vpn_admin_and_udp_mapping(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        nft = (self.base / "applied.nft").read_text()
        self.assertIn("seed 0x58525031 0-0 ct mark set (ct mark & 0xf0ffffff) | 16777216", nft)
        self.assertIn("seed 0x58525031 1-3 ct mark set (ct mark & 0xf0ffffff) | 33554432", nft)
        self.assertIn("numgen random mod 4 < 1", nft)
        self.assertIn("ct mark & 0x0f000000 == 16777216 meta mark set (meta mark & 0xf0ffffff) | 16777216 return", nft)
        self.assertIn("ct mark & 0x0f000000 == 33554432 meta mark set (meta mark & 0xf0ffffff) | 33554432 return", nft)
        self.assertIn("ct mark & 0x0f000000 != 0 drop", nft)
        self.assertIn("jhash ip saddr . udp sport mod 4 seed", nft)
        self.assertNotIn("jhash ip daddr", nft)
        self.assertIn("meta mark & 0x200 != 0 return", nft)
        self.assertIn("ip saddr 192.168.1.42 return", nft)
        self.assertIn("ip daddr 203.0.113.0/24 return", nft)
        self.assertIn('nat iifname "br-lan" oifname "lan1"', nft)
        self.assertLess(nft.index("ct mark & 0x0f000000 != 0"), nft.index("jhash"))
        self.assertLess(nft.index("ct state established,related return"), nft.index("jhash"))
        self.assertTrue(any("oif" in call and "lan1" in call and "211" in call for call in self.calls()))
        self.assertFalse(any("flush" in call or ("replace" in call and "main" in call) for call in self.calls()))

    def test_health_down_preserves_existing_default_but_excludes_new_flows(self):
        self.assertEqual(self.run_script().returncode, 0)
        before = len(self.calls())
        result = self.run_script(["--down", "wan"])
        self.assertEqual(result.returncode, 0, result.stderr)
        nft = (self.base / "applied.nft").read_text()
        self.assertIn("seed 0x58525031 0-2 ct mark set (ct mark & 0xf0ffffff) | 33554432", nft)
        self.assertNotIn("seed 0x58525031 0-0 ct mark set (ct mark & 0xf0ffffff) | 16777216", nft)
        self.assertIn("ct mark & 0x0f000000 == 16777216", nft)
        self.assertNotIn("numgen", nft)
        self.assertFalse(any(call[:4] == ["ip", "-4", "rule", "del"] for call in self.calls()[before:]))
        self.assertFalse(any(call[:4] == ["ip", "-4", "route", "del"] for call in self.calls()[before:]))

    def test_carrier_or_missing_address_excluded_with_stable_marks(self):
        self.cfg["carrier_down"] = ["wan"]
        self.assertEqual(self.run_script().returncode, 0)
        self.assertIn("seed 0x58525031 0-2 ct mark set (ct mark & 0xf0ffffff) | 33554432", (self.base / "applied.nft").read_text())
        self.cfg["no_address"] = ["lan1"]
        self.assertEqual(self.run_script().returncode, 0)
        nft = (self.base / "applied.nft").read_text()
        self.assertIn("select drop", nft)
        self.assertNotIn("numgen", nft)

    def test_owned_old_routes_are_not_reused_after_lease_loss(self):
        self.cfg["routes"] = "default via 198.51.100.1 dev wan\ndefault via 192.168.0.99 dev lan1 table 211\n"
        self.assertEqual(self.run_script().returncode, 0)
        nft = (self.base / "applied.nft").read_text()
        self.assertNotIn("seed 0x58525031 1-3", nft)
        self.assertIn("mod 1", nft)
        self.assertFalse(any("192.168.0.99" in call for call in self.calls()))

    def test_cleanup_removes_only_reserved_owned_state(self):
        self.assertEqual(self.run_script().returncode, 0)
        before = len(self.calls())
        self.assertEqual(self.run_script(cleanup=True).returncode, 0)
        calls = self.calls()[before:]
        flushes = [call for call in calls if "flush" in call]
        self.assertEqual(flushes, [["ip", "-4", "route", "flush", "table", "210"],
                                  ["ip", "-4", "route", "flush", "table", "211"]])
        before = len(self.calls())
        self.assertEqual(self.run_script(cleanup=True).returncode, 0)
        self.assertEqual(len(self.calls()), before)

    def test_invalid_input_and_foreign_state_fail_without_mutation(self):
        self.assertNotEqual(self.run_script(["--member", "bad;name:1"]).returncode, 0)
        self.assertFalse((self.base / "calls").exists())
        self.cfg["occupied"] = "unreachable default\n"
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse((self.base / "applied.nft").exists())
        self.assertFalse(any("add" in call or "replace" in call for call in self.calls()))

    def test_nft_check_failure_does_not_replace_existing_rules(self):
        self.cfg["nft_fail"] = True
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertTrue((self.base / "checked.nft").exists())
        self.assertFalse((self.base / "applied.nft").exists())

    def test_ipv4_cidr_bypass_is_applied_and_ipv6_is_not_put_in_ipv4_nft(self):
        result = self.run_script(["--bypass", "192.168.1.139/24", "--bypass", "2001:db8::/64", "--bypass", "::ffff:192.0.2.1"])
        self.assertEqual(result.returncode, 0, result.stderr)
        nft = (self.base / "applied.nft").read_text()
        self.assertIn("ip saddr 192.168.1.0/24 return", nft)
        self.assertNotIn("2001:db8", nft)
        self.assertNotIn("::ffff", nft)

    def test_one_member_and_large_weight_are_supported(self):
        result = self.run_script(members=["--member", "wan:4294967295"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("mod 4294967295", (self.base / "applied.nft").read_text())

    def test_distinct_alias_device_names_are_not_mistaken_for_duplicates(self):
        result = self.run_script(members=["--member", "wan:1:1", "--member", "wan:3"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("seed 0x58525031 0-2 ct mark set (ct mark & 0xf0ffffff) | 33554432", (self.base / "applied.nft").read_text())


if __name__ == "__main__":
    unittest.main()
