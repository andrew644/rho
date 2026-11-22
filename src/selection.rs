//! Rectangle selection over the rendered screen, used for mouse copy.
//!
//! Selections are stored in absolute terminal cell coordinates because that is
//! what mouse events report. Copying reads the cells back out of the screen
//! snapshot taken by [`screen_from_buffer`], so wrapped transcript lines,
//! indented tool output and code blocks all copy exactly as they are shown.

use ratatui::buffer::Buffer;
use ratatui::text::Span;

/// Marks a cell that continues a wide glyph (or an empty symbol); it has no text of its own.
pub const CONT: char = '\0';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub col: u16,
    pub row: u16,
}

impl Anchor {
    pub fn new(col: u16, row: u16) -> Self {
        Self { col, row }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started (or where a click landed).
    pub start: Anchor,
    /// The moving end of the selection.
    pub end: Anchor,
    /// First selectable column of the region the selection lives in.
    pub first_col: u16,
    /// Last selectable column of the region the selection lives in.
    pub last_col: u16,
}

impl Selection {
    pub fn new(start: Anchor, end: Anchor, first_col: u16, last_col: u16) -> Self {
        let last_col = last_col.max(first_col);
        Self {
            start,
            end,
            first_col,
            last_col,
        }
    }

    pub fn point(at: Anchor, first_col: u16, last_col: u16) -> Self {
        Self::new(at, at, first_col, last_col)
    }

    /// The two ends in reading order (top to bottom, then left to right).
    pub fn ordered(&self) -> (Anchor, Anchor) {
        if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    pub fn top(&self) -> u16 {
        self.ordered().0.row
    }

    pub fn bottom(&self) -> u16 {
        self.ordered().1.row
    }

    pub fn is_point(&self) -> bool {
        self.start == self.end
    }

    /// Inclusive columns covered on `row`, or `None` when the row is untouched.
    ///
    /// Rows between the two ends are covered edge to edge, like a terminal
    /// charwise selection; the end rows are clipped to the ends themselves.
    pub fn row_range(&self, row: u16) -> Option<(u16, u16)> {
        let (first, last) = self.ordered();
        if row < first.row || row > last.row {
            return None;
        }
        let lo = if row == first.row {
            first.col
        } else {
            self.first_col
        };
        let hi = if row == last.row {
            last.col
        } else {
            self.last_col
        };
        Some((lo.min(hi), lo.max(hi)))
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        self.row_range(row)
            .is_some_and(|(lo, hi)| col >= lo && col <= hi)
    }

    /// Move both ends `delta` rows down, keeping the same text when the view scrolls.
    pub fn shift_rows(&mut self, delta: i32, top: u16, bottom: u16) {
        if bottom < top {
            return;
        }
        let shift = |row: u16| (row as i32 + delta).clamp(top as i32, bottom as i32) as u16;
        self.start.row = shift(self.start.row);
        self.end.row = shift(self.end.row);
    }
}

/// Snapshot of the cells of a rendered frame, one entry per terminal column.
pub fn screen_from_buffer(buffer: &Buffer) -> Vec<Vec<char>> {
    let area = buffer.area;
    let width = area.width as usize;
    let mut rows = Vec::with_capacity(area.height as usize);
    for row in 0..area.height {
        let mut cells: Vec<char> = Vec::with_capacity(width);
        let mut col: u16 = 0;
        while col < area.width {
            let symbol = buffer[(col, row)].symbol();
            let glyph_width = Span::raw(symbol).width();
            if glyph_width == 0 {
                cells.push(CONT);
                col += 1;
                continue;
            }
            let first = symbol.chars().find(|c| *c != CONT).unwrap_or(' ');
            cells.push(first);
            cells.resize(cells.len() + glyph_width - 1, CONT);
            col = col.saturating_add(glyph_width as u16);
        }
        cells.truncate(width);
        cells.resize(width, ' ');
        rows.push(cells);
    }
    rows
}

/// The text covered by `selection`, with row padding trimmed and blank edge rows dropped.
pub fn extract(screen: &[Vec<char>], selection: &Selection) -> String {
    let mut rows: Vec<String> = Vec::new();
    for row in selection.top()..=selection.bottom() {
        let Some(cells) = screen.get(row as usize) else {
            continue;
        };
        let Some((lo, hi)) = selection.row_range(row) else {
            continue;
        };
        let mut line = String::new();
        for col in lo..=hi {
            match cells.get(col as usize) {
                Some(&CONT) | None => {}
                Some(&c) => line.push(c),
            }
        }
        let line = line.trim_end().to_string();
        if line.is_empty() && rows.is_empty() {
            continue;
        }
        rows.push(line);
    }
    while rows.last().is_some_and(|line| line.is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Punct,
}

fn classify(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn real_column(cells: &[char], col: usize) -> Option<usize> {
    let mut col = col.min(cells.len().saturating_sub(1));
    loop {
        match *cells.get(col)? {
            CONT if col > 0 => col -= 1,
            CONT => return None,
            _ => return Some(col),
        }
    }
}

/// Inclusive column range of the run of like characters covering `(row, col)`.
pub fn word_at(screen: &[Vec<char>], row: usize, col: usize) -> Option<(usize, usize)> {
    let cells = screen.get(row)?;
    if cells.is_empty() {
        return None;
    }
    let anchor = real_column(cells, col)?;
    let class = classify(*cells.get(anchor)?);
    let mut lo = anchor;
    while lo > 0 {
        let prev = cells[lo - 1];
        if prev != CONT && classify(prev) != class {
            break;
        }
        lo -= 1;
    }
    let mut hi = anchor;
    while hi + 1 < cells.len() {
        let next = cells[hi + 1];
        if next != CONT && classify(next) != class {
            break;
        }
        hi += 1;
    }
    Some((lo, hi))
}

/// Inclusive column range of a whole row, clipped to the selectable region.
pub fn line_at(
    screen: &[Vec<char>],
    row: usize,
    first_col: usize,
    last_col: usize,
) -> Option<(usize, usize)> {
    let cells = screen.get(row)?;
    if cells.is_empty() {
        return None;
    }
    let lo = first_col.min(cells.len().saturating_sub(1));
    let hi = last_col.min(cells.len().saturating_sub(1));
    Some((lo.min(hi), lo.max(hi)))
}

#[cfg(test)]
pub fn screen_from_lines(lines: &[&str], width: usize) -> Vec<Vec<char>> {
    lines
        .iter()
        .map(|line| {
            let mut cells: Vec<char> = line.chars().collect();
            cells.resize(width, ' ');
            cells
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn screen() -> Vec<Vec<char>> {
        screen_from_lines(
            &[
                "alpha beta gamma",
                "one two three",
                "   indented tail",
                "",
                "last line here",
            ],
            20,
        )
    }

    fn sel(sc: u16, sr: u16, ec: u16, er: u16) -> Selection {
        Selection::new(Anchor::new(sc, sr), Anchor::new(ec, er), 0, 19)
    }

    #[test]
    fn row_range_clips_end_rows_and_spans_middle_rows() {
        let selection = sel(6, 1, 10, 3);
        assert_eq!(selection.row_range(0), None);
        assert_eq!(selection.row_range(1), Some((6, 19)));
        assert_eq!(selection.row_range(2), Some((0, 19)));
        assert_eq!(selection.row_range(3), Some((0, 10)));
        assert_eq!(selection.row_range(4), None);
        assert!(selection.contains(15, 2));
        assert!(!selection.contains(15, 3));
        assert!(!selection.contains(2, 1));
    }

    #[test]
    fn drag_down_right_copies_a_block() {
        let text = extract(&screen(), &sel(6, 0, 9, 2));
        assert_eq!(text, "beta gamma\none two three\n   indente");
    }

    #[test]
    fn drag_up_left_copies_the_same_block() {
        let text = extract(&screen(), &sel(9, 2, 6, 0));
        assert_eq!(text, "beta gamma\none two three\n   indente");
    }

    #[test]
    fn blank_edge_rows_and_padding_are_dropped() {
        let text = extract(&screen(), &sel(0, 0, 19, 4));
        assert_eq!(
            text,
            "alpha beta gamma\none two three\n   indented tail\n\nlast line here"
        );
        let trailing = extract(&screen(), &sel(8, 1, 19, 1));
        assert_eq!(trailing, "three");
    }

    #[test]
    fn a_single_cell_selection_yields_that_cell() {
        let text = extract(&screen(), &sel(0, 1, 0, 1));
        assert_eq!(text, "o");
        assert!(sel(0, 3, 0, 3).is_point());
    }

    #[test]
    fn shift_rows_keeps_the_same_text_when_scrolling() {
        let mut selection = sel(2, 1, 8, 2);
        selection.shift_rows(3, 0, 9);
        assert_eq!(selection.start.row, 4);
        assert_eq!(selection.end.row, 5);
        selection.shift_rows(-10, 0, 9);
        assert_eq!(selection.start.row, 0);
        assert_eq!(selection.end.row, 0);
    }

    #[test]
    fn word_at_expands_over_matching_characters() {
        let s = screen();
        assert_eq!(word_at(&s, 0, 2), Some((0, 4)));
        assert_eq!(word_at(&s, 0, 7), Some((6, 9)));
        assert_eq!(word_at(&s, 2, 5), Some((3, 10)));
        assert_eq!(word_at(&s, 2, 1), Some((0, 2)));
    }

    #[test]
    fn word_at_stays_inside_the_clicked_row() {
        let s = screen();
        assert_eq!(word_at(&s, 3, 0), Some((0, 19)));
        assert_eq!(word_at(&s, 0, 19), Some((16, 19)));
    }

    #[test]
    fn line_at_spans_the_selectable_region() {
        let s = screen();
        assert_eq!(line_at(&s, 1, 2, 12), Some((2, 12)));
        assert_eq!(line_at(&s, 1, 2, 400), Some((2, 19)));
    }

    #[test]
    fn wide_glyphs_occupy_two_cells_and_copy_as_one_char() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_stringn(0, 0, "日 ok", 6, Style::default());
        let s = screen_from_buffer(&buffer);
        assert_eq!(s[0][0], '日');
        assert_eq!(s[0][1], CONT);
        assert_eq!(s[0][2], ' ');
        assert_eq!(extract(&s, &sel(0, 0, 5, 0)), "日 ok");
        assert_eq!(word_at(&s, 0, 1), Some((0, 1)));
        assert_eq!(word_at(&s, 0, 3), Some((3, 4)));
    }

    #[test]
    fn screen_from_buffer_keeps_cell_columns_aligned() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
        buffer.set_stringn(0, 0, "ab", 4, Style::default());
        let s = screen_from_buffer(&buffer);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].len(), 4);
        assert_eq!(s[0].iter().collect::<String>(), "ab  ");
        assert_eq!(s[1].iter().collect::<String>(), "    ");
    }
}
