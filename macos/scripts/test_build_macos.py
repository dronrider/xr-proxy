import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class BuildMacosTest(unittest.TestCase):
    def check_build(self, config, flags):
        with tempfile.TemporaryDirectory(prefix="xr-build-test-") as temporary:
            base = Path(temporary).resolve()
            root = base / "source tree"
            script = root / "macos/scripts/build-macos.sh"
            script.parent.mkdir(parents=True)
            script.write_bytes(Path(__file__).with_name("build-macos.sh").read_bytes())
            for directory in ("XrCore", "XrMac"):
                (root / "macos" / directory).mkdir()
            bin_dir = base / "bin"
            bin_dir.mkdir()
            log = base / "commands.jsonl"
            stub = f"#!{sys.executable}\n" + r'''
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
with open(os.environ["XR_BUILD_TEST_LOG"], "a") as output:
    output.write(json.dumps([name, sys.argv[1:], os.environ.get("MACOSX_DEPLOYMENT_TARGET")]) + "\n")
if name in ("codesign", "xcrun", "ditto", "spctl"):
    sys.exit("unexpected signing command")
if name == "xcodebuild":
    args = sys.argv[1:]
    if "-derivedDataPath" not in args:
        sys.exit("missing explicit build directory")
    products = Path(args[args.index("-derivedDataPath") + 1]) / "Build/Products"
    configuration = args[args.index("-configuration") + 1]
    (products / configuration / "XrMac.app").mkdir(parents=True)
'''
            for command in ("cargo", "swift", "xcodegen", "xcodebuild", "codesign", "xcrun", "ditto", "spctl"):
                executable = bin_dir / command
                executable.write_text(stub)
                executable.chmod(0o755)
            env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ["PATH"],
                       XR_BUILD_TEST_LOG=str(log), CONFIG="debug")
            env.pop("MACOSX_DEPLOYMENT_TARGET", None)
            result = subprocess.run(["sh", str(script), *flags], cwd=base, env=env,
                                    capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual([call[0] for call in calls], ["cargo", "swift", "xcodegen", "xcodebuild"])
            self.assertTrue(all(call[2] == "13.0" for call in calls), calls)
            cargo_args = ["build", "--manifest-path", str(root / "Cargo.toml"), "-p", "xr-ios-ffi"]
            if config == "Release":
                cargo_args.append("--release")
            self.assertEqual(calls[0][1], cargo_args)
            self.assertEqual(calls[1][1], ["run", "xrcore-tests"])
            self.assertEqual(calls[2][1], ["generate"])
            self.assertEqual(calls[3][1], [
                "-project", str(root / "macos/XrMac/XrMac.xcodeproj"), "-scheme", "XrMac",
                "-configuration", config, "-destination", "platform=macOS",
                "-derivedDataPath", str(root / "target/macos"),
                "LIBRARY_SEARCH_PATHS=" + str(root / "target" / config.lower()),
                "CODE_SIGNING_ALLOWED=NO", "build",
            ])
            app = root / "target/macos/Build/Products" / config / "XrMac.app"
            self.assertIn("==> Сборка: " + str(app), result.stdout)

    def test_debug_without_signing(self):
        self.check_build("Debug", [])

    def test_release_without_signing(self):
        self.check_build("Release", ["--release"])


if __name__ == "__main__":
    unittest.main()
