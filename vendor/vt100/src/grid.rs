use crate::term::BufWrite as _;

#[derive(Clone, Debug)]
pub struct Grid {
    size: Size,
    pos: Pos,
    saved_pos: Pos,
    rows: Vec<crate::row::Row>,
    scroll_top: u16,
    scroll_bottom: u16,
    origin_mode: bool,
    saved_origin_mode: bool,
    scrollback: std::collections::VecDeque<crate::row::Row>,
    scrollback_len: usize,
    scrollback_offset: usize,
    // zaivern patch: **最上段から始まるスクロール領域**の押し出しも履歴へ積むか。
    //
    // 実端末 (alacritty / xterm.js 系) は「領域が row 0 から始まるなら、部分領域
    // でも押し出した行を履歴へ積む」。codex のような inline TUI はこの意味論に
    // 依存して履歴を流す (`[1;29r` + LF で上端から押し出し、下端の入力欄は固定)。
    // 通常画面のグリッドだけ true にする (screen.rs)。**代替画面は false のまま** —
    // vim は最下段のステータス行を残すため領域を `[1;rows-1 r` と**上端固定**で
    // 取るので、ここを true にすると**画面を 1 行スクロールするたびに履歴へ積む**
    // ことになる (alacritty も代替画面の履歴上限は 0 で、同じく積まない)。
    pub(crate) save_region_scrolls: bool,
}

impl Grid {
    pub fn new(size: Size, scrollback_len: usize) -> Self {
        Self {
            size,
            pos: Pos::default(),
            saved_pos: Pos::default(),
            rows: vec![],
            scroll_top: 0,
            scroll_bottom: size.rows.saturating_sub(1),
            origin_mode: false,
            saved_origin_mode: false,
            scrollback: std::collections::VecDeque::new(),
            scrollback_len,
            scrollback_offset: 0,
            save_region_scrolls: false,
        }
    }

    pub fn allocate_rows(&mut self) {
        if self.rows.is_empty() {
            self.rows.extend(
                std::iter::repeat_with(|| {
                    crate::row::Row::new(self.size.cols)
                })
                .take(usize::from(self.size.rows)),
            );
        }
    }

    fn new_row(&self) -> crate::row::Row {
        crate::row::Row::new(self.size.cols)
    }

    pub fn clear(&mut self) {
        self.pos = Pos::default();
        self.saved_pos = Pos::default();
        for row in self.drawing_rows_mut() {
            row.clear(crate::attrs::Attrs::default());
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.size.rows.saturating_sub(1);
        self.origin_mode = false;
        self.saved_origin_mode = false;
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn set_size(&mut self, size: Size) {
        if size.cols != self.size.cols {
            for row in &mut self.rows {
                row.wrap(false);
            }
        }

        if self.scroll_bottom == self.size.rows.saturating_sub(1) {
            self.scroll_bottom = size.rows.saturating_sub(1);
        }

        self.size = size;
        for row in &mut self.rows {
            row.resize(size.cols, crate::cell::Cell::default());
        }

        // zaivern patch: 行数が減るとき、元実装は `Vec::resize` で**末尾から**
        // 行を捨てていた。末尾はカーソルのある側 — つまり TUI がいま描いている
        // 内容そのものなので、画面を縮めた瞬間に本文が消える。しかも増やし直しても
        // 空行が足されるだけで戻らない (子プロセスは差分描画しかしないため、
        // 消えた行を二度と送ってこない)。Cockpit でファイルを開いてペインが
        // 縮むと、その端末だけが黒いまま戻らなくなる原因がこれ。
        //
        // 実端末と同じく「あふれた分は上から履歴へ送る」に直す。まずカーソルより
        // 下の余白を落とし、それでも足りない分だけ先頭から送り出す
        // (こうするとカーソル行とその上は必ず残る)。
        let old_rows = self.rows.len();
        let new_rows = usize::from(size.rows);
        if !self.rows.is_empty() && new_rows < old_rows {
            let below_cursor =
                old_rows.saturating_sub(usize::from(self.pos.row) + 1);
            let from_top = (old_rows - new_rows).saturating_sub(below_cursor);
            for _ in 0..from_top {
                let removed = self.rows.remove(0);
                // 代替画面 (scrollback_len == 0) には履歴が無いので捨てるだけ。
                // それでもカーソル行が残る点が元実装との違い。
                if self.scrollback_len > 0 {
                    self.scrollback.push_back(removed);
                    while self.scrollback.len() > self.scrollback_len {
                        self.scrollback.pop_front();
                    }
                    if self.scrollback_offset > 0 {
                        self.scrollback_offset =
                            self.scrollback.len().min(self.scrollback_offset + 1);
                    }
                }
            }
            self.pos.row = self
                .pos
                .row
                .saturating_sub(u16::try_from(from_top).unwrap_or(u16::MAX));
            // 保存カーソル (DECSC) も同じだけ内容がずれる
            self.saved_pos.row = self
                .saved_pos
                .row
                .saturating_sub(u16::try_from(from_top).unwrap_or(u16::MAX));
        }
        self.rows.resize(new_rows, self.new_row());

        if self.scroll_bottom >= size.rows {
            self.scroll_bottom = size.rows.saturating_sub(1);
        }
        if self.scroll_bottom < self.scroll_top {
            self.scroll_top = 0;
        }

        // zaivern patch: saved_pos (DECSC) も必ず新しい画面内へ収める。
        // ?1049h (保存) → 縮小 → ?1049l (DECRC 復元) の順で範囲外の行を
        // 復元すると、直後の描画で current_row_mut() の unwrap が panic し、
        // PTY 読取スレッドだけが死んで端末が黒いまま戻らなくなる。
        self.saved_pos.row = self.saved_pos.row.min(size.rows.saturating_sub(1));
        self.saved_pos.col = self.saved_pos.col.min(size.cols.saturating_sub(1));

        self.row_clamp_top(false);
        self.row_clamp_bottom(false);
        self.col_clamp();
    }

    pub fn pos(&self) -> Pos {
        self.pos
    }

    pub fn set_pos(&mut self, mut pos: Pos) {
        if self.origin_mode {
            pos.row = pos.row.saturating_add(self.scroll_top);
        }
        self.pos = pos;
        self.row_clamp_top(self.origin_mode);
        self.row_clamp_bottom(self.origin_mode);
        self.col_clamp();
    }

    pub fn save_cursor(&mut self) {
        self.saved_pos = self.pos;
        self.saved_origin_mode = self.origin_mode;
    }

    pub fn restore_cursor(&mut self) {
        self.pos = self.saved_pos;
        self.origin_mode = self.saved_origin_mode;
        // zaivern patch: 保存後に画面が縮んでいた場合の保険 (set_size 側の
        // クランプと二重化)。範囲外カーソルを復元すると current_row_mut() の
        // unwrap が panic する。
        self.pos.row = self.pos.row.min(self.size.rows.saturating_sub(1));
        self.pos.col = self.pos.col.min(self.size.cols.saturating_sub(1));
    }

    pub fn visible_rows(&self) -> impl Iterator<Item = &crate::row::Row> {
        // zaivern patch: 元実装は `rows_len - scrollback_offset` を計算するため
        // offset が画面行数を超えると debug ビルドで減算オーバーフローし、
        // release でも取り過ぎた行を返していた。チェーン全体を rows_len で
        // 打ち切れば、任意の offset (<= scrollback.len()) で正しい窓になる。
        let scrollback_len = self.scrollback.len();
        let rows_len = self.rows.len();
        self.scrollback
            .iter()
            .skip(scrollback_len - self.scrollback_offset)
            .chain(self.rows.iter())
            .take(rows_len)
    }

    pub fn drawing_rows(&self) -> impl Iterator<Item = &crate::row::Row> {
        self.rows.iter()
    }

    pub fn drawing_rows_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut crate::row::Row> {
        self.rows.iter_mut()
    }

    pub fn visible_row(&self, row: u16) -> Option<&crate::row::Row> {
        self.visible_rows().nth(usize::from(row))
    }

    pub fn drawing_row(&self, row: u16) -> Option<&crate::row::Row> {
        self.drawing_rows().nth(usize::from(row))
    }

    pub fn drawing_row_mut(
        &mut self,
        row: u16,
    ) -> Option<&mut crate::row::Row> {
        self.drawing_rows_mut().nth(usize::from(row))
    }

    /// zaivern patch: 元実装は `unwrap()` だった。「カーソル行は必ず存在する」
    /// という不変条件は 0 行グリッドや行未確保の状態で普通に破れ、破れたときに
    /// 死ぬのは PTY 読取スレッド — その端末は二度と更新されず、parser の Mutex も
    /// poison してタイルが黒いまま戻らなくなる。Option にして呼び出し側で諦める。
    pub fn current_row_mut(&mut self) -> Option<&mut crate::row::Row> {
        self.drawing_row_mut(self.pos.row)
    }

    pub fn visible_cell(&self, pos: Pos) -> Option<&crate::cell::Cell> {
        self.visible_row(pos.row).and_then(|r| r.get(pos.col))
    }

    pub fn drawing_cell(&self, pos: Pos) -> Option<&crate::cell::Cell> {
        self.drawing_row(pos.row).and_then(|r| r.get(pos.col))
    }

    pub fn drawing_cell_mut(
        &mut self,
        pos: Pos,
    ) -> Option<&mut crate::cell::Cell> {
        self.drawing_row_mut(pos.row)
            .and_then(|r| r.get_mut(pos.col))
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback_len
    }

    pub fn scrollback(&self) -> usize {
        self.scrollback_offset
    }

    /// zaivern patch: 履歴だけを捨てる。`clear()` は表示行しか触らないので、
    /// 代替画面へ入り直したとき (DECSET 1049) に**前のアプリの履歴が残る**。
    /// 代替画面にも履歴を持たせた以上、入り直しの度にここで捨てる必要がある。
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.scrollback_offset = 0;
    }

    pub fn set_scrollback(&mut self, rows: usize) {
        self.scrollback_offset = rows.min(self.scrollback.len());
    }

    pub fn write_contents(&self, contents: &mut String) {
        let mut wrapping = false;
        for row in self.visible_rows() {
            row.write_contents(contents, 0, self.size.cols, wrapping);
            if !row.wrapped() {
                contents.push('\n');
            }
            wrapping = row.wrapped();
        }

        while contents.ends_with('\n') {
            contents.truncate(contents.len() - 1);
        }
    }

    pub fn write_contents_formatted(
        &self,
        contents: &mut Vec<u8>,
    ) -> crate::attrs::Attrs {
        crate::term::ClearAttrs::default().write_buf(contents);
        crate::term::ClearScreen::default().write_buf(contents);

        let mut prev_attrs = crate::attrs::Attrs::default();
        let mut prev_pos = Pos::default();
        let mut wrapping = false;
        for (i, row) in self.visible_rows().enumerate() {
            // we limit the number of cols to a u16 (see Size), so
            // visible_rows() can never return more rows than will fit
            let i = i.try_into().unwrap();
            let (new_pos, new_attrs) = row.write_contents_formatted(
                contents,
                0,
                self.size.cols,
                i,
                wrapping,
                Some(prev_pos),
                Some(prev_attrs),
            );
            prev_pos = new_pos;
            prev_attrs = new_attrs;
            wrapping = row.wrapped();
        }

        self.write_cursor_position_formatted(
            contents,
            Some(prev_pos),
            Some(prev_attrs),
        );

        prev_attrs
    }

    pub fn write_contents_diff(
        &self,
        contents: &mut Vec<u8>,
        prev: &Self,
        mut prev_attrs: crate::attrs::Attrs,
    ) -> crate::attrs::Attrs {
        let mut prev_pos = prev.pos;
        let mut wrapping = false;
        let mut prev_wrapping = false;
        for (i, (row, prev_row)) in
            self.visible_rows().zip(prev.visible_rows()).enumerate()
        {
            // we limit the number of cols to a u16 (see Size), so
            // visible_rows() can never return more rows than will fit
            let i = i.try_into().unwrap();
            let (new_pos, new_attrs) = row.write_contents_diff(
                contents,
                prev_row,
                0,
                self.size.cols,
                i,
                wrapping,
                prev_wrapping,
                prev_pos,
                prev_attrs,
            );
            prev_pos = new_pos;
            prev_attrs = new_attrs;
            wrapping = row.wrapped();
            prev_wrapping = prev_row.wrapped();
        }

        self.write_cursor_position_formatted(
            contents,
            Some(prev_pos),
            Some(prev_attrs),
        );

        prev_attrs
    }

    pub fn write_cursor_position_formatted(
        &self,
        contents: &mut Vec<u8>,
        prev_pos: Option<Pos>,
        prev_attrs: Option<crate::attrs::Attrs>,
    ) {
        let prev_attrs = prev_attrs.unwrap_or_default();
        // writing a character to the last column of a row doesn't wrap the
        // cursor immediately - it waits until the next character is actually
        // drawn. it is only possible for the cursor to have this kind of
        // position after drawing a character though, so if we end in this
        // position, we need to redraw the character at the end of the row.
        if prev_pos != Some(self.pos) && self.pos.col >= self.size.cols {
            let mut pos = Pos {
                row: self.pos.row,
                col: self.size.cols.saturating_sub(1),
            };
            // zaivern patch: 元実装は「最終列の升は必ずある / 全角の左半分も
            // 必ずある」と決め打って unwrap + `cols - 2` していた。1 桁端末では
            // `cols - 2` が桁溢れし、行が未確保なら unwrap が None を踏む。
            // どちらも描画中 (UI スレッド) の panic になるため Option で扱う。
            if self
                .drawing_cell(pos)
                .is_some_and(crate::cell::Cell::is_wide_continuation)
            {
                pos.col = self.size.cols.saturating_sub(2);
            }
            let Some(cell) = self.drawing_cell(pos) else {
                return;
            };
            if cell.has_contents() {
                if let Some(prev_pos) = prev_pos {
                    crate::term::MoveFromTo::new(prev_pos, pos)
                        .write_buf(contents);
                } else {
                    crate::term::MoveTo::new(pos).write_buf(contents);
                }
                cell.attrs().write_escape_code_diff(contents, &prev_attrs);
                contents.extend(cell.contents().as_bytes());
                prev_attrs.write_escape_code_diff(contents, cell.attrs());
            } else {
                // if the cell doesn't have contents, we can't have gotten
                // here by drawing a character in the last column. this means
                // that as far as i'm aware, we have to have reached here from
                // a newline when we were already after the end of an earlier
                // row. in the case where we are already after the end of an
                // earlier row, we can just write a few newlines, otherwise we
                // also need to do the same as above to get ourselves to after
                // the end of a row.
                let mut found = false;
                for i in (0..self.pos.row).rev() {
                    pos.row = i;
                    pos.col = self.size.cols.saturating_sub(1);
                    // zaivern patch: 上と同じ理由で Option 扱いにする。
                    if self
                        .drawing_cell(pos)
                        .is_some_and(crate::cell::Cell::is_wide_continuation)
                    {
                        pos.col = self.size.cols.saturating_sub(2);
                    }
                    let Some(cell) = self.drawing_cell(pos) else {
                        continue;
                    };
                    if cell.has_contents() {
                        if let Some(prev_pos) = prev_pos {
                            if prev_pos.row != i
                                || prev_pos.col < self.size.cols
                            {
                                crate::term::MoveFromTo::new(prev_pos, pos)
                                    .write_buf(contents);
                                cell.attrs().write_escape_code_diff(
                                    contents,
                                    &prev_attrs,
                                );
                                contents.extend(cell.contents().as_bytes());
                                prev_attrs.write_escape_code_diff(
                                    contents,
                                    cell.attrs(),
                                );
                            }
                        } else {
                            crate::term::MoveTo::new(pos).write_buf(contents);
                            cell.attrs().write_escape_code_diff(
                                contents,
                                &prev_attrs,
                            );
                            contents.extend(cell.contents().as_bytes());
                            prev_attrs.write_escape_code_diff(
                                contents,
                                cell.attrs(),
                            );
                        }
                        contents.extend(
                            "\n".repeat(usize::from(self.pos.row - i))
                                .as_bytes(),
                        );
                        found = true;
                        break;
                    }
                }

                // this can happen if you get the cursor off the end of a row,
                // and then do something to clear the end of the current row
                // without moving the cursor (IL, DL, ED, EL, etc). we know
                // there can't be something in the last column because we
                // would have caught that above, so it should be safe to
                // overwrite it.
                if !found {
                    pos = Pos {
                        row: self.pos.row,
                        col: self.size.cols.saturating_sub(1),
                    };
                    if let Some(prev_pos) = prev_pos {
                        crate::term::MoveFromTo::new(prev_pos, pos)
                            .write_buf(contents);
                    } else {
                        crate::term::MoveTo::new(pos).write_buf(contents);
                    }
                    contents.push(b' ');
                    // we know that the cell has no contents, but it still may
                    // have drawing attributes (background color, etc)
                    // zaivern patch: 升が無ければ末尾の再描画は諦める。
                    let Some(end_cell) = self.drawing_cell(pos) else {
                        return;
                    };
                    end_cell
                        .attrs()
                        .write_escape_code_diff(contents, &prev_attrs);
                    crate::term::SaveCursor::default().write_buf(contents);
                    crate::term::Backspace::default().write_buf(contents);
                    crate::term::EraseChar::new(1).write_buf(contents);
                    crate::term::RestoreCursor::default().write_buf(contents);
                    prev_attrs
                        .write_escape_code_diff(contents, end_cell.attrs());
                }
            }
        } else if let Some(prev_pos) = prev_pos {
            crate::term::MoveFromTo::new(prev_pos, self.pos)
                .write_buf(contents);
        } else {
            crate::term::MoveTo::new(self.pos).write_buf(contents);
        }
    }

    pub fn erase_all(&mut self, attrs: crate::attrs::Attrs) {
        for row in self.drawing_rows_mut() {
            row.clear(attrs);
        }
    }

    pub fn erase_all_forward(&mut self, attrs: crate::attrs::Attrs) {
        let pos = self.pos;
        for row in self.drawing_rows_mut().skip(usize::from(pos.row) + 1) {
            row.clear(attrs);
        }

        self.erase_row_forward(attrs);
    }

    pub fn erase_all_backward(&mut self, attrs: crate::attrs::Attrs) {
        let pos = self.pos;
        for row in self.drawing_rows_mut().take(usize::from(pos.row)) {
            row.clear(attrs);
        }

        self.erase_row_backward(attrs);
    }

    pub fn erase_row(&mut self, attrs: crate::attrs::Attrs) {
        if let Some(row) = self.current_row_mut() {
            row.clear(attrs);
        }
    }

    pub fn erase_row_forward(&mut self, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let Some(row) = self.current_row_mut() else { return };
        for col in pos.col..size.cols {
            row.erase(col, attrs);
        }
    }

    pub fn erase_row_backward(&mut self, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let Some(row) = self.current_row_mut() else { return };
        for col in 0..=pos.col.min(size.cols.saturating_sub(1)) {
            row.erase(col, attrs);
        }
    }

    /// zaivern patch: **繰り返し回数を「意味のある上限」で頭打ちにする**。
    ///
    /// CSI の引数は 65535 まで取れる。元実装はその回数だけ素朴に回すため、
    /// 例えば `CSI 65535 @` (ICH) は 200 桁の行に対して 65535 回の
    /// `Vec::insert` (各 O(cols)) を回し、**1 個のエスケープで数秒**かかっていた。
    /// これは PTY 読取スレッドが parser の Mutex を握ったまま起きるので、
    /// 同じフレームで描画しようとした UI スレッドがその間ずっと待たされる
    /// = **アプリ全体が固まる**。タイルを閉じた直後など、端末が縮んで TUI が
    /// 全画面を描き直すときに実際に踏み得る。
    ///
    /// 上限を超えた繰り返しは結果を変えない (対象領域はすべて空白になり切る/
    /// あふれた分は最後に切り捨てられる) ので、頭打ちにしても描画は同じ。
    pub fn insert_cells(&mut self, count: u16) {
        let size = self.size;
        let pos = self.pos;
        // 行の残り桁数を超えて挿入しても、末尾の truncate で消えるだけ。
        let count = count.min(size.cols.saturating_sub(pos.col));
        let wide = pos.col < size.cols
            && self
                .drawing_cell(pos)
                .is_some_and(crate::cell::Cell::is_wide_continuation);
        let Some(row) = self.current_row_mut() else { return };
        for _ in 0..count {
            // zaivern patch: 升が無ければ全角フラグの付け外しは飛ばす。
            if wide {
                if let Some(c) = row.get_mut(pos.col) {
                    c.set_wide_continuation(false);
                }
            }
            row.insert(pos.col, crate::cell::Cell::default());
            if wide {
                if let Some(c) = row.get_mut(pos.col) {
                    c.set_wide_continuation(true);
                }
            }
        }
        row.truncate(size.cols);
    }

    pub fn delete_cells(&mut self, count: u16) {
        let size = self.size;
        let pos = self.pos;
        let Some(row) = self.current_row_mut() else { return };
        // zaivern patch: 折返し待ち (pos.col == cols) や縮小直後は
        // `cols - pos.col` が桁溢れする。
        for _ in 0..(count.min(size.cols.saturating_sub(pos.col))) {
            row.remove(pos.col);
        }
        row.resize(size.cols, crate::cell::Cell::default());
    }

    pub fn erase_cells(&mut self, count: u16, attrs: crate::attrs::Attrs) {
        let size = self.size;
        let pos = self.pos;
        let Some(row) = self.current_row_mut() else { return };
        for col in pos.col..((pos.col.saturating_add(count)).min(size.cols)) {
            row.erase(col, attrs);
        }
    }

    pub fn insert_lines(&mut self, count: u16) {
        // 領域が全部空行になり切ったら、それ以上回しても結果は同じ。
        let count = count.min(
            self.scroll_bottom
                .saturating_sub(self.pos.row)
                .saturating_add(1),
        );
        for _ in 0..count {
            // zaivern patch: 0 行グリッドや領域外の scroll_bottom で範囲外になる
            if usize::from(self.scroll_bottom) >= self.rows.len() {
                return;
            }
            self.rows.remove(usize::from(self.scroll_bottom));
            self.rows.insert(usize::from(self.pos.row), self.new_row());
            // self.scroll_bottom is maintained to always be a valid row
            self.rows[usize::from(self.scroll_bottom)].wrap(false);
        }
    }

    pub fn delete_lines(&mut self, count: u16) {
        // zaivern patch: `rows - pos.row` は縮小直後に桁溢れする。
        // 併せて「領域を空にし切る回数」で頭打ちにする (insert_cells の説明を参照)。
        let count = count
            .min(self.size.rows.saturating_sub(self.pos.row))
            .min(
                self.scroll_bottom
                    .saturating_sub(self.pos.row)
                    .saturating_add(1),
            );
        for _ in 0..count {
            self.rows
                .insert(usize::from(self.scroll_bottom) + 1, self.new_row());
            self.rows.remove(usize::from(self.pos.row));
        }
    }

    pub fn scroll_up(&mut self, count: u16) {
        // zaivern patch: `rows - scroll_top` は縮小直後に桁溢れする。
        let count = count.min(self.size.rows.saturating_sub(self.scroll_top));
        for _ in 0..count {
            self.rows
                .insert(usize::from(self.scroll_bottom) + 1, self.new_row());
            let removed = self.rows.remove(usize::from(self.scroll_top));
            // zaivern patch: 全画面のスクロールに加えて、**最上段 (row 0) から
            // 始まる領域**の押し出しも履歴へ積む (`save_region_scrolls` のとき)。
            // 実端末はこの形を履歴に積むので、codex 等の inline TUI が
            // 「入力欄を下端に固定したまま上から履歴を流す」のに使っている。
            // 途中の行から始まる領域 (vim の下側分割など) はどの端末でも
            // 積まないので、従来どおり捨てる。
            let saves = self.scrollback_len > 0
                && (!self.scroll_region_active()
                    || (self.save_region_scrolls && self.scroll_top == 0));
            if saves {
                self.scrollback.push_back(removed);
                while self.scrollback.len() > self.scrollback_len {
                    self.scrollback.pop_front();
                }
                if self.scrollback_offset > 0 {
                    self.scrollback_offset =
                        self.scrollback.len().min(self.scrollback_offset + 1);
                }
            }
        }
    }

    pub fn scroll_down(&mut self, count: u16) {
        // zaivern patch: 領域を空にし切る回数で頭打ち (insert_cells の説明を参照)。
        let count = count.min(
            self.scroll_bottom
                .saturating_sub(self.scroll_top)
                .saturating_add(1),
        );
        for _ in 0..count {
            // zaivern patch: 0 行グリッドや領域外の scroll_bottom で範囲外になる
            if usize::from(self.scroll_bottom) >= self.rows.len() {
                return;
            }
            self.rows.remove(usize::from(self.scroll_bottom));
            self.rows
                .insert(usize::from(self.scroll_top), self.new_row());
            // self.scroll_bottom is maintained to always be a valid row
            self.rows[usize::from(self.scroll_bottom)].wrap(false);
        }
    }

    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let bottom = bottom.min(self.size().rows.saturating_sub(1));
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.size().rows.saturating_sub(1);
        }
        self.pos.row = self.scroll_top;
        self.pos.col = 0;
    }

    fn in_scroll_region(&self) -> bool {
        self.pos.row >= self.scroll_top && self.pos.row <= self.scroll_bottom
    }

    fn scroll_region_active(&self) -> bool {
        self.scroll_top != 0 || self.scroll_bottom != self.size.rows.saturating_sub(1)
    }

    pub fn set_origin_mode(&mut self, mode: bool) {
        self.origin_mode = mode;
        self.set_pos(Pos { row: 0, col: 0 });
    }

    pub fn row_inc_clamp(&mut self, count: u16) {
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_add(count);
        self.row_clamp_bottom(in_scroll_region);
    }

    pub fn row_inc_scroll(&mut self, count: u16) -> u16 {
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_add(count);
        let lines = self.row_clamp_bottom(in_scroll_region);
        if in_scroll_region {
            self.scroll_up(lines);
            lines
        } else {
            0
        }
    }

    pub fn row_dec_clamp(&mut self, count: u16) {
        let in_scroll_region = self.in_scroll_region();
        self.pos.row = self.pos.row.saturating_sub(count);
        self.row_clamp_top(in_scroll_region);
    }

    pub fn row_dec_scroll(&mut self, count: u16) {
        let in_scroll_region = self.in_scroll_region();
        // need to account for clamping by both row_clamp_top and by
        // saturating_sub
        let extra_lines = if count > self.pos.row {
            count - self.pos.row
        } else {
            0
        };
        self.pos.row = self.pos.row.saturating_sub(count);
        let lines = self.row_clamp_top(in_scroll_region);
        self.scroll_down(lines + extra_lines);
    }

    pub fn row_set(&mut self, i: u16) {
        self.pos.row = i;
        self.row_clamp();
    }

    pub fn col_inc(&mut self, count: u16) {
        self.pos.col = self.pos.col.saturating_add(count);
    }

    pub fn col_inc_clamp(&mut self, count: u16) {
        self.pos.col = self.pos.col.saturating_add(count);
        self.col_clamp();
    }

    pub fn col_dec(&mut self, count: u16) {
        self.pos.col = self.pos.col.saturating_sub(count);
    }

    pub fn col_tab(&mut self) {
        self.pos.col -= self.pos.col % 8;
        self.pos.col += 8;
        self.col_clamp();
    }

    pub fn col_set(&mut self, i: u16) {
        self.pos.col = i;
        self.col_clamp();
    }

    pub fn col_wrap(&mut self, width: u16, wrap: bool) {
        if self.pos.col > self.size.cols.saturating_sub(width) {
            let mut prev_pos = self.pos;
            self.pos.col = 0;
            let scrolled = self.row_inc_scroll(1);
            // zaivern patch: 行数より多くスクロールし得る (1 行の端末や、
            // 縮小直後にカーソルがスクロール領域の外にいる場合) ため、
            // 引き算は飽和させる。debug では桁溢れ panic、release では
            // 65535 行目という嘘の行番号になって直後の unwrap が死ぬ。
            prev_pos.row = prev_pos.row.saturating_sub(scrolled);
            let new_pos = self.pos;
            // zaivern patch: 行が確保される前 (allocate_rows 前) や範囲外だと
            // None が返る。ここで unwrap すると PTY 読取スレッドだけが落ちて
            // 端末が黒いまま固まるので、折返しフラグを諦めるだけにする。
            if let Some(row) = self.drawing_row_mut(prev_pos.row) {
                row.wrap(wrap && prev_pos.row.saturating_add(1) == new_pos.row);
            }
        }
    }

    fn row_clamp_top(&mut self, limit_to_scroll_region: bool) -> u16 {
        if limit_to_scroll_region && self.pos.row < self.scroll_top {
            let rows = self.scroll_top - self.pos.row;
            self.pos.row = self.scroll_top;
            rows
        } else {
            0
        }
    }

    fn row_clamp_bottom(&mut self, limit_to_scroll_region: bool) -> u16 {
        let bottom = if limit_to_scroll_region {
            self.scroll_bottom
        } else {
            self.size.rows.saturating_sub(1)
        };
        if self.pos.row > bottom {
            let rows = self.pos.row - bottom;
            self.pos.row = bottom;
            rows
        } else {
            0
        }
    }

    fn row_clamp(&mut self) {
        if self.pos.row > self.size.rows.saturating_sub(1) {
            self.pos.row = self.size.rows.saturating_sub(1);
        }
    }

    fn col_clamp(&mut self) {
        if self.pos.col > self.size.cols.saturating_sub(1) {
            self.pos.col = self.size.cols.saturating_sub(1);
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Pos {
    pub row: u16,
    pub col: u16,
}
