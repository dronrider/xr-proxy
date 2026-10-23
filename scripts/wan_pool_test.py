#!/usr/bin/env python3
"""Run the pool scripts against a stateful, isolated ip/nft command fixture."""
import ipaddress
import json
import os
import shlex
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
    print(cfg.get('routes' if table == 'all' else 'main' if table == 'main' else 'occupied_' + table, cfg.get('occupied', '')), end='')
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
                                  ["ip", "-4", "route", "flush", "table", "211"],
                                  ["ip", "-4", "route", "flush", "table", "218"]])
        before = len(self.calls())
        self.assertEqual(self.run_script(cleanup=True).returncode, 0)
        self.assertEqual(len(self.calls()), before)

    def fallback_routes(self, calls=None):
        return [call for call in (self.calls() if calls is None else calls)
                if call[:5] == ["ip", "-4", "route", "replace", "default"] and call[-2:] == ["table", "218"]]

    def test_dhcp_loss_keeps_router_and_bypass_default_on_second_wan(self):
        self.cfg["routes"] = "default via 192.0.2.1 dev lan1 table 202\n"
        self.cfg["main"] = "10.10.10.0/24 dev wg0\n"
        self.cfg["no_address"] = ["wan"]
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.fallback_routes(), [["ip", "-4", "route", "replace", "default",
                         "via", "192.0.2.1", "dev", "lan1", "onlink", "metric", "10", "table", "218"]])
        calls = self.calls()
        self.assertIn(["ip", "-4", "rule", "add", "priority", "32763", "not", "fwmark", "0x200/0x200", "lookup", "main", "suppress_prefixlength", "0"], calls)
        self.assertIn(["ip", "-4", "rule", "add", "priority", "32764", "not", "fwmark", "0x200/0x200", "lookup", "218"], calls)
        self.assertIn(["ip", "-4", "rule", "add", "priority", "11100", "oif", "wan", "lookup", "210"], calls)
        self.assertIn(["ip", "-4", "route", "replace", "unreachable", "default", "metric", "32767", "table", "210"], calls)
        self.assertEqual((self.base / "state/fallback-selected").read_text(), "lan1\n")

    def test_raw_forward_bypass_is_masqueraded_without_pool_mark(self):
        self.cfg["routes"] = "default via 192.0.2.1 dev lan1 table 202\n"
        self.cfg["main"] = "203.0.113.0/24 dev wg0\n"
        self.cfg["no_address"] = ["wan"]
        self.assertEqual(self.run_script().returncode, 0)
        nft = (self.base / "applied.nft").read_text()
        bypass = "add rule ip xr_wan_pool select ip saddr 192.168.1.42 return"
        self.assertIn(bypass, nft)
        self.assertLess(nft.index(bypass), nft.index("ct mark set"))
        fallback = self.fallback_routes()[-1]
        egress = fallback[fallback.index("dev") + 1]
        nat = [shlex.split(line) for line in nft.splitlines()
               if line.startswith("add rule ip xr_wan_pool nat ")]

        def forwarded_source(source, ingress, output, mark=0, earlier_binding=None, protocol="tcp", port=22):
            if earlier_binding is not None:
                return earlier_binding
            for rule in nat:
                if rule[rule.index("iifname") + 1] != ingress or rule[rule.index("oifname") + 1] != output:
                    continue
                if "l4proto" in rule and rule[rule.index("l4proto") + 1] != protocol:
                    continue
                if ("tcp" in rule and protocol != "tcp") or ("udp" in rule and protocol != "udp"):
                    continue
                if "dport" in rule and int(rule[rule.index("dport") + 1]) != port:
                    continue
                if "mark" in rule:
                    start = rule.index("mark")
                    mask = int(rule[start + 2], 0)
                    expected = int(rule[start + 4], 0)
                    actual = mark & mask
                    comparison = rule[start + 3]
                    if (comparison == "!=" and actual == expected) or (comparison == "==" and actual != expected):
                        continue
                if rule[-1] == "masquerade":
                    return {"wan": "198.51.100.4", "lan1": "192.0.2.4"}[output]
            return source

        # Raw forwarding enters here after redirect bypass; no xr-client socket or
        # outer TLS tunnel supplies a WAN source address on its behalf.
        for protocol, port in (("tcp", 22), ("tcp", 8443), ("udp", 53)):
            with self.subTest(protocol=protocol, port=port):
                self.assertEqual(forwarded_source("192.168.1.42", "br-lan", egress, mark=0, protocol=protocol, port=port), "192.0.2.4")
        self.assertEqual(forwarded_source("192.168.1.43", "br-lan", egress, mark=0x2000000), "192.0.2.4")
        self.assertEqual(forwarded_source("192.168.1.42", None, egress), "192.168.1.42")
        self.assertEqual(forwarded_source("192.168.1.42", "br-lan", "wg0"), "192.168.1.42")
        self.assertEqual(forwarded_source("203.0.113.8", "lan1", "br-lan"), "203.0.113.8")
        self.assertEqual(forwarded_source("192.168.1.42", "br-lan", "br-lan"), "192.168.1.42")
        self.assertEqual(forwarded_source("192.168.1.42", "br-lan", "wan", earlier_binding="198.51.100.4"), "198.51.100.4")

    def test_late_tproxy_and_destination_policy_survive_router_fallback(self):
        # The router can install TPROXY at 32765 after reboot, rather than 9999.
        (self.base / "rules").write_text(
            "10000: from 192.0.2.4 lookup 202\n"
            "20000: from all to 192.0.2.4/24 lookup 202\n"
            "32765: from all fwmark 0x200 lookup 201\n"
            "32766: from all lookup main\n")
        self.assertEqual(self.run_script(["--down", "wan"]).returncode, 0)
        rules = sorted((line.split() for line in (self.base / "rules").read_text().splitlines()),
                       key=lambda fields: int(fields[0][:-1]))

        def lookup(mark, destination, source="198.51.100.20"):
            for rule in rules:
                if "fwmark" in rule:
                    value = rule[rule.index("fwmark") + 1].split("/")
                    mask = int(value[1], 0) if len(value) == 2 else 0xffffffff
                    matches = mark & mask == int(value[0], 0)
                    if "not" in rule:
                        matches = not matches
                    if not matches:
                        continue
                if "from" in rule:
                    src = rule[rule.index("from") + 1]
                    if src != "all" and ipaddress.ip_address(source) not in ipaddress.ip_network(src, strict=False):
                        continue
                if "to" in rule and ipaddress.ip_address(destination) not in ipaddress.ip_network(rule[rule.index("to") + 1], strict=False):
                    continue
                if "oif" in rule:
                    continue
                table = rule[rule.index("lookup") + 1]
                if table == "main" and "suppress_prefixlength" in rule:
                    if ipaddress.ip_address(destination) not in ipaddress.ip_network("203.0.113.0/24"):
                        continue
                return table
            return None

        self.assertEqual(lookup(0x200, "198.51.100.80"), "201")
        self.assertEqual(lookup(0x200, "203.0.113.80"), "201")
        self.assertEqual(lookup(0, "198.51.100.80"), "218")
        self.assertEqual(lookup(0, "203.0.113.80"), "main")
        self.assertEqual(lookup(0, "192.0.2.80"), "202")
        self.assertEqual(lookup(0, "198.51.100.80", source="192.0.2.4"), "202")
        self.assertIn("32765: from all fwmark 0x200 lookup 201", (self.base / "rules").read_text())

    def test_health_failure_overrides_stale_primary_default_and_recovers(self):
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.fallback_routes()[-1][8], "wan")
        before = len(self.calls())
        self.assertEqual(self.run_script(["--down", "wan"]).returncode, 0)
        self.assertEqual(self.fallback_routes(self.calls()[before:])[-1][8], "lan1")
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.fallback_routes()[-1][8], "wan")
        self.assertEqual(sum(call[:6] == ["ip", "-4", "rule", "add", "priority", "32764"] for call in self.calls()), 1)
        self.assertFalse(any("main" in call and ("replace" in call or "flush" in call or "del" in call) for call in self.calls()))

    def test_all_down_closes_unbound_default_and_does_not_reuse_owned_route(self):
        self.cfg["routes"] += "default via 192.0.2.99 dev wan table 218\n"
        self.assertEqual(self.run_script(["--down", "wan", "--down", "lan1"]).returncode, 0)
        self.assertFalse(self.fallback_routes())
        self.assertIn(["ip", "-4", "route", "del", "default", "metric", "10", "table", "218"], self.calls())
        self.assertIn(["ip", "-4", "route", "replace", "unreachable", "default", "metric", "32767", "table", "218"], self.calls())
        self.assertEqual((self.base / "state/fallback-selected").read_text(), "none\n")
        self.cfg["routes"] = "default via 192.0.2.99 dev wan table 218\n"
        self.assertEqual(self.run_script().returncode, 0)
        self.assertFalse(self.fallback_routes())

    def test_foreign_fallback_table_and_rules_are_not_claimed(self):
        self.cfg["occupied_218"] = "default via 192.0.2.99 dev other\n"
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse(any("add" in call or "replace" in call or "flush" in call for call in self.calls()))
        self.assertFalse((self.base / "state/fallback-owned").exists())
        self.cfg.pop("occupied_218")
        (self.base / "rules").write_text("32763: from all lookup custom\n")
        self.assertNotEqual(self.run_script().returncode, 0)
        self.assertFalse((self.base / "state/fallback-owned").exists())

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
