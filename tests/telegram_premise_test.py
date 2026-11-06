#!/usr/bin/env python3
"""Offline premise labels on captain cards; no Telegram or fleet access."""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "plugins" / "telegram"))
from door import premise_text


class PremiseCards(unittest.TestCase):
    def test_task_and_terminal_outcomes(self):
        mail = {"premise": {"kind": "task", "ref": "work"}}
        self.assertEqual(premise_text(mail), "Premise: task work")
        for outcome in ("done", "cancelled"):
            text = premise_text({**mail, "premise_outcome": outcome})
            self.assertIn(outcome, text)
            self.assertIn("ask may be stale", text)
            self.assertIn("No answer or approval was changed", text)

    def test_forge_references_are_explicitly_unobserved(self):
        for kind in ("pr", "issue"):
            self.assertEqual(premise_text({"premise": {"kind": kind, "repo": "acme/project", "number": 7}}),
                             f"Premise: {kind} acme/project#7 (not observed)")
        self.assertEqual(premise_text({}), "")


if __name__ == "__main__":
    unittest.main()
