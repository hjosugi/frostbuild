import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent / "scripts"))

import spam_guard  # noqa: E402

OWNER = "hjosugi"

# Real bodies from the campaign observed on issues #2, #11, and #27.
CAMPAIGN_BODIES = [
    "(https://github.com/bebamocewo/performance_patch_v2/releases/download/release/performance_patch_v2.zip)",
    "(https://github.com/begukovuma/release_fix_2.1.0/releases/download/release/release_fix_2.1.0.zip)",
    "(https://github.com/bacowuvamena33/critical_patch_2026/releases/download/release/critical_patch_2026.zip)",
]


class ClassifyTest(unittest.TestCase):
    def test_campaign_bodies_are_detected(self) -> None:
        for body in CAMPAIGN_BODIES:
            reasons = spam_guard.classify(body, "NONE", OWNER)
            self.assertTrue(reasons, body)
            self.assertTrue(
                any("unrelated repository" in r for r in reasons), reasons
            )

    def test_bare_archive_link_with_lure_name_is_detected(self) -> None:
        body = "https://example.com/downloads/critical_patch_2026.zip"
        self.assertTrue(spam_guard.classify(body, "NONE", OWNER))

    def test_trusted_authors_are_never_flagged(self) -> None:
        for association in ("OWNER", "MEMBER", "COLLABORATOR"):
            for body in CAMPAIGN_BODIES:
                self.assertEqual(spam_guard.classify(body, association, OWNER), [])

    def test_plain_text_comment_is_ignored(self) -> None:
        body = "header変更で依存TUのみ再compileされることを確認しました。"
        self.assertEqual(spam_guard.classify(body, "NONE", OWNER), [])

    def test_link_with_surrounding_prose_is_ignored(self) -> None:
        body = (
            "Benchmark results are attached, see "
            "https://github.com/other/repo/releases/download/v1/results.zip "
            "for the raw data."
        )
        self.assertEqual(spam_guard.classify(body, "NONE", OWNER), [])

    def test_non_archive_link_only_comment_is_ignored(self) -> None:
        body = "(https://github.com/hjosugi/frost-build/pull/40)"
        self.assertEqual(spam_guard.classify(body, "NONE", OWNER), [])

    def test_own_repo_release_still_flagged_as_link_only_archive(self) -> None:
        body = "(https://github.com/hjosugi/frost-build/releases/download/v1/frost.zip)"
        reasons = spam_guard.classify(body, "NONE", OWNER)
        self.assertTrue(reasons)
        self.assertFalse(any("unrelated repository" in r for r in reasons))


if __name__ == "__main__":
    unittest.main()
