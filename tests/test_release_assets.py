"""Release asset coverage: every triple the installers can request must be built.

install.sh constructs TARGET as "<arch>-<os>" for
  arch in {x86_64, aarch64} x os in {apple-darwin, unknown-linux-gnu},
postinstall.js TARGET_MAP adds x86_64-pc-windows-msvc (zip).
release.yml must build all of them, otherwise ARM Linux (and any future
mapped triple) 404s at install time.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def release_targets():
    text = (ROOT / ".github/workflows/release.yml").read_text()
    return set(re.findall(r"target:\s*([a-z0-9_.\-]+)", text))


def postinstall_triples():
    text = (ROOT / "scripts/postinstall.js").read_text()
    found = set(re.findall(r"'([a-z0-9_]+-[a-z0-9_.\-]+)'", text))
    # Keep only real target triples (os suffix), drop incidental strings
    # like 'agent-debugger-npm-installer'.
    return {t for t in found if re.search(r"(darwin|linux-gnu|windows-msvc)", t)}


class TestReleaseAssetCoverage(unittest.TestCase):
    def test_linux_arm_is_built(self):
        self.assertIn(
            "aarch64-unknown-linux-gnu",
            release_targets(),
            "release.yml must build aarch64-unknown-linux-gnu "
            "(install.sh + postinstall.js already request it)",
        )

    def test_installer_triples_all_built(self):
        # install.sh: 2 arch x 2 os = 4 tar.gz triples.
        install_triples = {
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        }
        # postinstall.js: install triples + windows zip.
        expected = install_triples | {"x86_64-pc-windows-msvc"}
        self.assertTrue(
            expected <= release_targets(),
            f"missing builds: {expected - release_targets()}",
        )
        self.assertTrue(
            postinstall_triples() <= release_targets(),
            f"postinstall requests unbuilt: {postinstall_triples() - release_targets()}",
        )

    def test_expected_build_count(self):
        # 4 unix tar.gz + 1 windows zip; update this test when adding a tier.
        self.assertEqual(len(release_targets()), 5)


if __name__ == "__main__":
    unittest.main()
