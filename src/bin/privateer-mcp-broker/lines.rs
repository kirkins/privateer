//! Newline-delimited reading with a hard cap: an oversize line is consumed
//! and reported, never held in memory, so the stream stays usable.
use std::io::{self, BufRead};

pub enum Line {
    Text(Vec<u8>),
    TooLong,
}

/// The next line without its newline, `None` at end of stream. A final line
/// without a newline still counts.
pub fn next(reader: &mut impl BufRead, max: usize) -> io::Result<Option<Line>> {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut seen = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(seen.then(|| finish(line, too_long)));
        }
        seen = true;
        let (chunk, used, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (&available[..at], at + 1, true),
            None => (available, available.len(), false),
        };
        if !too_long {
            if line.len() + chunk.len() > max {
                too_long = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        reader.consume(used);
        if done {
            return Ok(Some(finish(line, too_long)));
        }
    }
}

fn finish(line: Vec<u8>, too_long: bool) -> Line {
    if too_long {
        Line::TooLong
    } else {
        Line::Text(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversize_line_is_skipped_and_the_next_one_read() {
        let mut input = io::BufReader::with_capacity(4, &b"ab\n0123456789\ncd"[..]);
        assert!(matches!(next(&mut input, 5).unwrap(), Some(Line::Text(l)) if l == b"ab"));
        assert!(matches!(next(&mut input, 5).unwrap(), Some(Line::TooLong)));
        assert!(matches!(next(&mut input, 5).unwrap(), Some(Line::Text(l)) if l == b"cd"));
        assert!(next(&mut input, 5).unwrap().is_none());
    }
}
