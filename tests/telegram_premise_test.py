#!/usr/bin/env python3
"""Offline premise labels on captain cards; no Telegram or fleet access."""
import pathlib
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "plugins" / "telegram"))
import door
from door import Door, premise_text


class PremiseCards(unittest.TestCase):
    def test_task_and_terminal_outcomes(self):
        mail = {"premise": {"kind": "task", "ref": "work"}}
        self.assertEqual(premise_text(mail), "Premise: task work")
        for outcome in ("done", "cancelled"):
            text = premise_text({**mail, "premise_outcome": outcome})
            self.assertIn(outcome, text)
            self.assertIn("ask may be stale", text)
            self.assertIn("No answer or approval was changed", text)

    def test_resolved_premise_updates_existing_card_and_preserves_buttons(self):
        for outcome in ("done", "cancelled"):
            card = {"message_id": 7, "text": "Review?", "markup": {"inline_keyboard": [[{"text": "Yes", "callback_data": "yes"}]]},
                    "also": [], "blocked": [], "members": []}
            cards = {"ask": card}
            payload = {"attached_to": "ask", "premise_changed": {
                "ask": "ask", "premise": {"kind": "task", "ref": "work"}, "outcome": outcome}}
            instance = Door.__new__(Door)
            instance.cfg = {}
            with patch.object(door.tg, "read_state", return_value=cards), \
                 patch.object(door.tg, "write_state") as write, \
                 patch.object(door.tg, "chat_id", return_value=1), \
                 patch.object(door.tg, "api", return_value=({}, "")) as api:
                self.assertTrue(instance.attach({"id": "change", "from": "apps", "kind": "notice"}, payload))
                sent = api.call_args.args[2]
                self.assertIn(f"Premise: task work — {outcome}", sent["text"])
                self.assertIn("ask may be stale", sent["text"])
                self.assertIn("No answer or approval was changed", sent["text"])
                self.assertIn('"callback_data": "yes"', sent["reply_markup"])
                self.assertIn("ask may be stale", write.call_args.args[1]["ask"]["also"][-1])
            self.assertNotIn("resolved", card)

    def test_forge_references_are_explicitly_unobserved(self):
        for kind in ("pr", "issue"):
            self.assertEqual(premise_text({"premise": {"kind": kind, "repo": "acme/project", "number": 7}}),
                             f"Premise: {kind} acme/project#7 (not observed)")
        self.assertEqual(premise_text({}), "")


if __name__ == "__main__":
    unittest.main()
