import unittest

from terminal_text import TerminalText


class TerminalEvidence(unittest.TestCase):
    def test_cursor_split_words_are_actual_contiguous_cells(self):
        text = TerminalText(4, 60)
        text.feed(b"\x1b[2;3HPlease\x1b[2;10Hanswer")
        self.assertTrue(text.contains("Please answer"))
        self.assertFalse(text.contains("Pleaseanswer"))
        separated = TerminalText(4, 60)
        separated.feed(b"\x1b[3;1HPlease\x1b[3;25Hanswer")
        self.assertFalse(separated.contains("Please answer"))

    def test_incomplete_escape_and_utf8_chunks_are_retained(self):
        text = TerminalText(4, 60)
        for byte in "\x1b[2;4HPlease answer 界é".encode():
            text.feed(bytes([byte]))
        self.assertTrue(text.contains("Please answer 界é"))
        text.feed(b"\x1b[2;")
        self.assertTrue(text.contains("Please answer 界é"))
        text.feed(b"4HNo\x1b[K")
        self.assertTrue(text.contains("No"))
        self.assertFalse(text.contains("Please answer"))

    def test_erasure_cell_reuse_and_changes_elsewhere_never_match_old_outcome(self):
        text = TerminalText(4, 60)
        text.feed(b"\x1b[2;1HRecorded answer")
        before = text.revision
        self.assertFalse(text.contains("Recorded answer", since=before))
        text.feed(b"\x1b[3;1Hnew attention\x1b[2;1HRecorded answer")
        self.assertFalse(text.contains("Recorded answer", since=before))
        text.feed(b"\x1b[2;1HRevalidating exact item\x1b[K")
        self.assertFalse(text.contains("Recorded answer"))
        text.feed(b"\x1b[2;1HRecorded answer\x1b[K")
        self.assertTrue(text.contains("Recorded answer", since=before))
        text.feed(b"\x1b[2;1H\x1b[2K")
        self.assertFalse(text.contains("Recorded answer"))
        text.feed(b"Recorded\x1b[3;1H answer")
        self.assertFalse(text.contains("Recorded answer"))
        text.feed(b"\x1b[2J")
        self.assertFalse(text.contains("Recorded"))

    def test_crossterm_mouse_reporting_modes_change_no_text_or_freshness(self):
        text = TerminalText(4, 60)
        text.feed(b"Recorded answer")
        before = text.revision
        text.feed(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h")
        text.feed(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l")
        self.assertTrue(text.contains("Recorded answer"))
        self.assertFalse(text.contains("Recorded answer", since=before))
        self.assertEqual(text.revision, before)
        with self.assertRaises(AssertionError):
            text.feed(b"\x1b[?9999h")

    def test_unsupported_controls_and_unbounded_escape_fail_closed(self):
        text = TerminalText(4, 60)
        with self.assertRaises(AssertionError):
            text.feed(b"\x1b[9z")
        text = TerminalText(4, 60)
        with self.assertRaises(AssertionError):
            text.feed(b"\x1b[" + b"1;" * 70)


if __name__ == "__main__":
    unittest.main()
