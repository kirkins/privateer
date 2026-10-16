"""Small bounded, cursor-aware terminal text observer for dashboard PTY tests.

Not a screen snapshot/layout assertion. Match current cells, never concatenated
escape-stripped output or old frames. Unsupported controls fail the proof closed.
"""
import codecs
import unicodedata


class TerminalText:
    def __init__(self, rows, columns):
        self.rows, self.columns = rows, columns
        self.cells = [[(" ", 0) for _ in range(columns)] for _ in range(rows)]
        self.row = self.column = self.revision = 0
        self.decoder = codecs.getincrementaldecoder("utf-8")("strict")
        self.escape = ""

    def _cell(self, row, column, text):
        if 0 <= row < self.rows and 0 <= column < self.columns:
            if self.cells[row][column][0] != text:
                self.revision += 1
                self.cells[row][column] = (text, self.revision)

    def _erase(self, first, last):
        for index in range(first, last):
            self._cell(index // self.columns, index % self.columns, " ")

    def _csi(self, sequence):
        body, command = sequence[2:-1], sequence[-1]
        if body.startswith("?"):
            assert command in "hl" and all(p in {"25", "1000", "1002", "1003", "1006", "1015", "1049", "2004", "2026", "7"} for p in body[1:].split(";")), sequence
            if body == "?1049" and command == "h":
                self._erase(0, self.rows * self.columns)
                self.row = self.column = 0
            return
        values = [int(part) if part else 0 for part in body.split(";")]
        n = values[0] or 1
        if command in "Hf":
            self.row = (values[0] or 1) - 1
            self.column = ((values[1] if len(values) > 1 else 1) or 1) - 1
        elif command == "G":
            self.column = n - 1
        elif command == "d":
            self.row = n - 1
        elif command == "A":
            self.row -= n
        elif command == "B":
            self.row += n
        elif command == "C":
            self.column += n
        elif command == "D":
            self.column -= n
        elif command in "EF":
            self.row += n if command == "E" else -n
            self.column = 0
        elif command in "JK":
            mode = values[0]
            assert mode in (0, 1, 2), sequence
            start = self.row * self.columns if command == "K" else 0
            end = (self.row + 1) * self.columns if command == "K" else self.rows * self.columns
            cursor = self.row * self.columns + self.column
            self._erase(start if mode in (1, 2) else cursor,
                        end if mode in (0, 2) else cursor + 1)
        else:
            assert command == "m", "unsupported terminal control: " + repr(sequence)
        self.row = min(max(self.row, 0), self.rows - 1)
        self.column = min(max(self.column, 0), self.columns - 1)

    def feed(self, data):
        for char in self.decoder.decode(data):
            if self.escape:
                self.escape += char
                assert len(self.escape) < 128, "unbounded terminal escape"
                if self.escape == "\x1b[":
                    continue
                assert self.escape.startswith("\x1b["), repr(self.escape)
                if "@" <= char <= "~":
                    self._csi(self.escape)
                    self.escape = ""
                continue
            if char == "\x1b":
                self.escape = char
            elif char == "\r":
                self.column = 0
            elif char == "\n":
                self.row += 1
                assert self.row < self.rows, "unsupported terminal scrolling"
            elif char == "\b":
                self.column = max(0, self.column - 1)
            elif char == "\t":
                self.column = min(self.columns - 1, (self.column // 8 + 1) * 8)
            else:
                assert ord(char) >= 32 and char != "\x7f", repr(char)
                if unicodedata.combining(char):
                    assert self.column > 0, "unbound combining mark"
                    previous = self.cells[self.row][self.column - 1][0]
                    self._cell(self.row, self.column - 1, previous + char)
                    continue
                width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
                assert self.column + width <= self.columns, "unsupported terminal wrapping"
                self._cell(self.row, self.column, char)
                if width == 2:
                    self._cell(self.row, self.column + 1, "")
                self.column += width

    def contains(self, text, since=None):
        """Current contiguous row text, optionally changed since a captured revision.

        Unchanged redraws and updates elsewhere cannot revive an old match.
        Spaces/cursor gaps are actual cells, not stripped escape boundaries.
        """
        for row in self.cells:
            value = "".join(cell[0] for cell in row)
            start = 0
            while (index := value.find(text, start)) >= 0:
                # Map Unicode text offsets back to cells (wide/combining text).
                offset = 0
                stamps = []
                for cell, stamp in row:
                    if offset < index + len(text) and offset + len(cell) > index:
                        stamps.append(stamp)
                    offset += len(cell)
                if since is None or any(stamp > since for stamp in stamps):
                    return True
                start = index + 1
        return False
