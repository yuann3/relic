//! Just enough of a VT100/xterm screen to read what `relic ui` drew: cursor
//! addressing, erase, the 1049 alternate screen and the cursor-visible mode.
//! Ratatui writes only changed cells between cursor moves, so the raw byte
//! stream does not contain the frame's text in order; this grid does.
//! Colors and attributes are parsed and dropped. Every char is one cell
//! (the fixture is ASCII; box drawing is single width).

pub struct Screen {
    rows: usize,
    cols: usize,
    main: Vec<Vec<char>>,
    alt: Vec<Vec<char>>,
    in_alt: bool,
    row: usize,
    col: usize,
    saved: (usize, usize),
    state: State,
    utf8: Vec<u8>,
    /// Answers owed to the program, as a real terminal would send them.
    replies: Vec<u8>,
}

enum State {
    Ground,
    Esc,
    /// ESC followed by an intermediate such as `(`: one more byte to drop.
    EscIntermediate,
    Csi(Vec<u8>),
    Osc,
    OscEsc,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        let (rows, cols) = (rows as usize, cols as usize);
        Self {
            rows,
            cols,
            main: blank(rows, cols),
            alt: blank(rows, cols),
            in_alt: false,
            row: 0,
            col: 0,
            saved: (0, 0),
            state: State::Ground,
            utf8: Vec::new(),
            replies: Vec::new(),
        }
    }

    pub fn in_alt(&self) -> bool {
        self.in_alt
    }

    /// The buffer currently displayed, one line per row, right-trimmed.
    pub fn text(&self) -> String {
        let grid = if self.in_alt { &self.alt } else { &self.main };
        grid.iter()
            .map(|r| r.iter().collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Ratatui's `Terminal::clear` asks for the cursor position (DSR) and
    /// gives up with an error after 2 s without an answer, so the harness
    /// must answer like a terminal.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows as usize, cols as usize);
        for grid in [&mut self.main, &mut self.alt] {
            grid.resize(rows, vec![' '; cols]);
            for r in grid.iter_mut() {
                r.resize(cols, ' ');
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        match std::mem::replace(&mut self.state, State::Ground) {
            State::Ground => self.ground(b),
            State::Esc => match b {
                b'[' => self.state = State::Csi(Vec::new()),
                b']' => self.state = State::Osc,
                b'7' => self.saved = (self.row, self.col),
                b'8' => (self.row, self.col) = self.saved,
                b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.state = State::EscIntermediate,
                _ => {}
            },
            State::EscIntermediate => {}
            State::Csi(mut p) => {
                if (0x40..=0x7e).contains(&b) {
                    self.csi(&p, b);
                } else {
                    p.push(b);
                    self.state = State::Csi(p);
                }
            }
            State::Osc => match b {
                0x07 => {}
                0x1b => self.state = State::OscEsc,
                _ => self.state = State::Osc,
            },
            State::OscEsc => {
                if b != b'\\' {
                    self.state = State::Osc;
                }
            }
        }
    }

    fn ground(&mut self, b: u8) {
        if b >= 0x80 {
            self.utf8.push(b);
            let want = match self.utf8[0] {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            if self.utf8.len() >= want {
                let c = std::str::from_utf8(&self.utf8)
                    .ok()
                    .and_then(|s| s.chars().next())
                    .unwrap_or('\u{FFFD}');
                self.utf8.clear();
                self.put(c);
            }
            return;
        }
        self.utf8.clear();
        match b {
            0x1b => self.state = State::Esc,
            b'\r' => self.col = 0,
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            0x08 => self.col = self.col.saturating_sub(1),
            0x20..=0x7e => self.put(b as char),
            _ => {}
        }
    }

    fn grid(&mut self) -> &mut Vec<Vec<char>> {
        if self.in_alt {
            &mut self.alt
        } else {
            &mut self.main
        }
    }

    /// xterm's pending wrap: writing the last column parks the cursor past
    /// it, and only the next printable char wraps.
    fn put(&mut self, c: char) {
        if self.col >= self.cols {
            self.col = 0;
            self.line_feed();
        }
        let (row, col) = (self.row, self.col);
        self.grid()[row][col] = c;
        self.col += 1;
    }

    fn line_feed(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            let cols = self.cols;
            let grid = self.grid();
            grid.remove(0);
            grid.push(vec![' '; cols]);
        }
    }

    fn csi(&mut self, params: &[u8], fin: u8) {
        let private = params.first() == Some(&b'?');
        let body = String::from_utf8_lossy(if private { &params[1..] } else { params });
        let nums: Vec<usize> = body
            .split(';')
            .map(|p| p.split(':').next().unwrap_or("").parse().unwrap_or(0))
            .collect();
        let n = |i: usize, default: usize| match nums.get(i) {
            Some(&0) | None => default,
            Some(&v) => v,
        };
        let (rows, cols) = (self.rows, self.cols);
        match (private, fin) {
            (false, b'H' | b'f') => {
                self.row = (n(0, 1) - 1).min(rows - 1);
                self.col = (n(1, 1) - 1).min(cols - 1);
            }
            (false, b'A') => self.row = self.row.saturating_sub(n(0, 1)),
            (false, b'B') => self.row = (self.row + n(0, 1)).min(rows - 1),
            (false, b'C') => self.col = (self.col + n(0, 1)).min(cols - 1),
            (false, b'D') => self.col = self.col.saturating_sub(n(0, 1)),
            (false, b'G') => self.col = (n(0, 1) - 1).min(cols - 1),
            (false, b'd') => self.row = (n(0, 1) - 1).min(rows - 1),
            (false, b'J') => {
                let (row, col) = (self.row, self.col.min(cols));
                let grid = self.grid();
                match nums.first().copied().unwrap_or(0) {
                    0 => {
                        grid[row][col..].fill(' ');
                        grid[row + 1..].iter_mut().for_each(|r| r.fill(' '));
                    }
                    1 => {
                        grid[..row].iter_mut().for_each(|r| r.fill(' '));
                        grid[row][..(col + 1).min(cols)].fill(' ');
                    }
                    _ => grid.iter_mut().for_each(|r| r.fill(' ')),
                }
            }
            (false, b'K') => {
                let (row, col) = (self.row, self.col.min(cols));
                let line = &mut self.grid()[row];
                match nums.first().copied().unwrap_or(0) {
                    0 => line[col..].fill(' '),
                    1 => line[..(col + 1).min(cols)].fill(' '),
                    _ => line.fill(' '),
                }
            }
            (false, b'n') if nums.first() == Some(&6) => {
                let reply = format!("\x1b[{};{}R", self.row + 1, self.col.min(cols - 1) + 1);
                self.replies.extend_from_slice(reply.as_bytes());
            }
            (false, b's') => self.saved = (self.row, self.col),
            (false, b'u') => (self.row, self.col) = self.saved,
            (true, b'h' | b'l') if nums.contains(&1049) => {
                let on = fin == b'h';
                if on && !self.in_alt {
                    self.saved = (self.row, self.col);
                    self.alt = blank(rows, cols);
                    self.in_alt = true;
                } else if !on && self.in_alt {
                    self.in_alt = false;
                    (self.row, self.col) = self.saved;
                }
            }
            _ => {}
        }
    }
}

fn blank(rows: usize, cols: usize) -> Vec<Vec<char>> {
    vec![vec![' '; cols]; rows]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressing_alt_screen_and_erase() {
        let mut s = Screen::new(3, 10);
        s.feed(b"shell$ \x1b[?1049h\x1b[2;3Hab\x1b[38;5;1mc\x1b[0m\x1b[1;1H\xe2\x94\x8cx");
        assert!(s.in_alt());
        assert_eq!(s.text(), "\u{250c}x\n  abc\n");
        s.feed(b"\x1b[2;4H\x1b[K");
        assert_eq!(s.text(), "\u{250c}x\n  a\n");
        s.feed(b"\x1b[?1049l");
        assert_eq!(s.text(), "shell$\n\n");
        s.feed(b"\r\nl2\r\nl3\r\nl4");
        assert_eq!(s.text(), "l2\nl3\nl4", "scrolls at the bottom");
        s.feed(b"\x1b[2;5H\x1b[6n");
        assert_eq!(s.take_replies(), b"\x1b[2;5R");
    }
}
