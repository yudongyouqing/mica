//! Scrollback search bar, tab rename bar, and clickable path/hyperlink detection.
#![allow(unsafe_op_in_unsafe_fn)]

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PathHover {
    pub(super) pane_id: u64,
    pub(super) line: i32,
    pub(super) column: usize,
    pub(super) cells: usize,
}

pub(super) fn path_hover_rect(
    hover: PathHover,
    pane_id: u64,
    rect: mica_core::layout::Rect,
    metrics: &FontMetrics,
    offset: usize,
    rows: u16,
) -> Option<mica_core::layout::Rect> {
    let view = hover.line + offset as i32;
    (hover.pane_id == pane_id && (0..i32::from(rows)).contains(&view)).then(|| {
        mica_core::layout::Rect::new(
            rect.x + hover.column as f32 * metrics.cell_width,
            rect.y + view as f32 * metrics.line_height,
            hover.cells as f32 * metrics.cell_width,
            metrics.line_height,
        )
    })
}

thread_local! {
    pub(super) static SEARCH_PANE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub(super) fn search_bar_geometry(
    r: mica_core::layout::Rect,
    m: &FontMetrics,
) -> (mica_core::layout::Rect, f32) {
    let h = (m.line_height + 4.0).min(r.h);
    let w = (52.0 * m.cell_width).min(r.w);
    (
        mica_core::layout::Rect::new(r.x, r.y + r.h - h, w, h),
        (m.cell_width * 3.0).min(w / 4.0),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchControl {
    Query,
    Previous,
    Next,
    Close,
}

thread_local! {
    static PRESSED: std::cell::Cell<Option<(u64, SearchControl)>> = const { std::cell::Cell::new(None) };
}

fn search_control_at(x: i32, y: i32) -> Option<(u64, SearchControl)> {
    if !SEARCH_OPEN.get() {
        return None;
    }
    let rects = active_pane_rects();
    TABS.with(|tabs| {
        let tabs = tabs.borrow();
        let tab = tabs.get(ACTIVE.get())?;
        let id = tab.active_pane_id()?;
        let (_, r) = rects.iter().find(|(p, _)| *p == id)?;
        let (bar, button) = search_bar_geometry(*r, &tab.active_pane_ref()?.metrics);
        let (x, y) = (x as f32, y as f32);
        let end = bar.x + bar.w;
        if x < bar.x || x >= end || y < bar.y || y >= bar.y + bar.h {
            return None;
        }
        let control = if x >= end - button {
            SearchControl::Close
        } else if x >= end - 2.0 * button {
            SearchControl::Next
        } else if x >= end - 3.0 * button {
            SearchControl::Previous
        } else {
            SearchControl::Query
        };
        Some((id, control))
    })
}

pub(super) fn search_press(hwnd: HWND, x: i32, y: i32) -> bool {
    let Some(hit) = search_control_at(x, y) else {
        return false;
    };
    PRESSED.set(Some(hit));
    unsafe {
        let _ = SetCapture(hwnd);
    }
    true
}

pub(super) fn cancel_search_press() {
    PRESSED.take();
}

pub(super) fn search_release(x: i32, y: i32) -> bool {
    let Some(pressed) = PRESSED.take() else {
        return false;
    };
    unsafe {
        let _ = ReleaseCapture();
    }
    if Some(pressed) == search_control_at(x, y) {
        match pressed.1 {
            SearchControl::Close => search_close(),
            SearchControl::Next => search_jump(true),
            SearchControl::Previous => search_jump(false),
            SearchControl::Query => {}
        }
    }
    true
}

/// #57 打开搜索:清查询/命中,首帧空条
pub(super) fn search_open() {
    SEARCH_PANE.set(TABS.with(|tabs| {
        tabs.borrow()
            .get(ACTIVE.get())
            .and_then(TabState::active_pane_id)
    }));
    SEARCH_OPEN.with(|o| o.set(true));
    SEARCH_QUERY.with(|q| q.borrow_mut().clear());
    SEARCH_MATCHES.with(|m| m.borrow_mut().clear());
    SEARCH_IDX.with(|i| i.set(0));
    draw_frame();
}

/// #57 关闭搜索
pub(super) fn search_close() {
    SEARCH_OPEN.with(|o| o.set(false));
    SEARCH_MATCHES.with(|m| m.borrow_mut().clear());
    draw_frame();
}

/// #57 重扫:查询变更后全 buffer(历史+视口)大小写不敏感匹配。
/// 命中表 = (buffer 行, 列, 宽)——列/宽均按**屏幕列**计(row_fold 的
/// span:宽字符 2 列,高亮矩形直接乘 cell_width);旧实现把小写化串的
/// 字节偏移当列、needle 字符数当宽,CJK/emoji/变长折叠(İ)三重错位。
/// buffer 行坐标与 jump_prompt 同构(Line 负 = 历史)。扫完跳首个命中
fn search_rescan() {
    let query = SEARCH_QUERY.with(|q| q.borrow().clone());
    let mut matches: Vec<(i32, usize, usize)> = Vec::new();
    if !query.is_empty() {
        let needle = mica_render::frame::fold_search(&query);
        TABS.with(|tabs| {
            let guard = tabs.borrow();
            if let Some(t) = guard
                .get(ACTIVE.get())
                .and_then(|tab| tab.active_pane_ref())
            {
                let (hist, rows, grid) = t.term.grid_dims();
                for l in -(hist as i32)..rows as i32 {
                    let (hay, spans) = mica_render::frame::row_fold(grid, l);
                    for (col, len) in mica_render::frame::find_row_matches(&hay, &spans, &needle) {
                        matches.push((l, col, len));
                    }
                }
            }
        });
    }
    SEARCH_MATCHES.with(|m| *m.borrow_mut() = matches);
    SEARCH_IDX.with(|i| i.set(0));
    // 首搜定位第一个命中(旧版误调 search_jump(false) 倒跳 n-1)
    search_show();
}

/// #57 定位到 SEARCH_IDX 当前命中(不推进游标):滚视口 + 重绘
fn search_show() {
    // 滚到 **SEARCH_IDX 当前**命中(首版误取 first()——Enter 步进后
    // 视口钉死在第一个命中,用户实报"不往下走")
    let target = SEARCH_MATCHES.with(|m| {
        let guard = m.borrow();
        SEARCH_IDX
            .with(std::cell::Cell::get)
            .checked_rem(guard.len())
            .and_then(|i| guard.get(i).map(|(l, _, _)| *l))
    });
    if let Some(line) = target {
        TABS.with(|tabs| {
            if let Some(t) = tabs
                .borrow_mut()
                .get_mut(ACTIVE.get())
                .and_then(|tab| tab.active_pane())
            {
                t.term.scroll_to_line(line);
                t.force_full = true;
            }
        });
    }
    draw_frame();
}

/// #57 跳命中:forward = 下一(Enter)/上一(Shift+Enter);跳到即滚视口
fn search_jump(forward: bool) {
    let n = SEARCH_MATCHES.with(|m| m.borrow().len());
    if n == 0 {
        draw_frame(); // 空命中也要刷条(计数显示 0/0)
        return;
    }
    let old = SEARCH_IDX.with(std::cell::Cell::get);
    let new = if forward {
        (old + 1) % n
    } else {
        (old + n.saturating_sub(1)) % n
    };
    SEARCH_IDX.with(|i| i.set(new));
    search_show();
}

/// #57 WM_CHAR 的搜索态拦截:消费则返回 true(调用方吞掉不进 pty)。
/// 可打印入查询;CR 跳下一;退格删字;Esc 关闭
pub(super) fn search_char(ch: char) -> bool {
    if !SEARCH_OPEN.with(std::cell::Cell::get) {
        return false;
    }
    match ch {
        '\r' => {
            search_jump(!current_mods().shift);
        }
        '\n' => {}
        '\u{8}' => {
            SEARCH_QUERY.with(|q| {
                q.borrow_mut().pop();
            });
            search_rescan();
        }
        '\u{1b}' => {
            search_close();
        }
        c if (c as u32) >= 0x20 => {
            SEARCH_QUERY.with(|q| {
                q.borrow_mut().push(c);
            });
            search_rescan();
        }
        _ => {}
    }
    true
}

/// #87 重命名态 WM_CHAR 拦截:同搜索条(Enter 确认/Esc 关/退格/可打印)
pub(super) fn rename_char(ch: char) -> bool {
    if !RENAME_OPEN.with(std::cell::Cell::get) {
        return false;
    }
    match ch {
        '\r' => {
            let name = RENAME_QUERY.with(|q| q.borrow().clone());
            let idx = RENAME_IDX.with(std::cell::Cell::get);
            TABS.with(|tabs| {
                if let Some(tab) = tabs.borrow_mut().get_mut(idx)
                    && !name.trim().is_empty()
                {
                    tab.title = name;
                }
            });
            RENAME_OPEN.with(|o| o.set(false));
            draw_frame();
        }
        '\x1b' => {
            RENAME_OPEN.with(|o| o.set(false));
            draw_frame();
        }
        '\u{8}' => {
            RENAME_QUERY.with(|q| {
                q.borrow_mut().pop();
            });
            draw_frame();
        }
        c if (c as u32) >= 0x20 => {
            RENAME_QUERY.with(|q| {
                q.borrow_mut().push(c);
            });
            draw_frame();
        }
        _ => {}
    }
    true
}

/// #57 WM_KEYDOWN 的搜索态拦截:Up/Down = 命中步进;消费返回 true
pub(super) fn search_key(vk: u32) -> bool {
    if !SEARCH_OPEN.with(std::cell::Cell::get) {
        return false;
    }
    if vk == VK_UP.0 as u32 {
        search_jump(false);
        return true;
    }
    if vk == VK_DOWN.0 as u32 {
        search_jump(true);
        return true;
    }
    false
}

/// #58 行内全部路径 span(静态下划线):token 遍历同构,收集所有
/// 通过模式判定的 token 的 (col, cells)——可点击路径常显下划线
pub(super) fn all_path_spans(text: &str, cell_of: &[usize]) -> Vec<(usize, usize)> {
    if !text.contains(['/', '\\']) {
        return Vec::new();
    }
    let bytes: Vec<char> = text.chars().collect();
    let is_word = |i: usize| {
        let c = bytes[i];
        c.is_alphanumeric() || matches!(c, '.' | '/' | '\\' | ':' | '_' | '-' | '+' | '@')
            // Combining characters belong to the preceding grid cell. Keep
            // their exact spelling instead of splitting a Unicode filename.
            || (i > 0 && cell_of.get(i).zip(cell_of.get(i-1)).is_some_and(|(a,b)| a == b))
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_word(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word(i) {
            i += 1;
        }
        let token: String = bytes[start..i].iter().collect();
        if normalize_path_token(&token).is_some() {
            let c0 = *cell_of.get(start).unwrap_or(&start);
            let end = cell_of
                .get(i)
                .copied()
                .unwrap_or_else(|| cell_of.get(i.saturating_sub(1)).copied().unwrap_or(c0) + 1);
            out.push((c0, end.saturating_sub(c0)));
        }
    }
    out
}

pub(super) fn update_hover_path(px: i32, py: i32) -> bool {
    let span = hover_path_span(px, py);
    HOVER_PATH.with(|h| {
        let mut old = h.borrow_mut();
        if *old == span {
            false
        } else {
            *old = span;
            true
        }
    })
}

fn hover_path_span(px: i32, py: i32) -> Option<PathHover> {
    let (pane_id, r) = active_pane_rects().into_iter().find(|(_, r)| {
        (px as f32) >= r.x
            && (px as f32) < r.x + r.w
            && (py as f32) >= r.y
            && (py as f32) < r.y + r.h
    })?;
    TABS.with(|tabs| {
        let guard = tabs.borrow();
        let t = &guard
            .get(ACTIVE.get())?
            .panes
            .iter()
            .find(|p| p.id == pane_id)?
            .terminal;
        let (point, _) = px_to_buffer_point(
            px as f32 - r.x,
            py as f32 - r.y,
            &t.metrics,
            t.term.display_offset(),
        );
        if point.column.0 >= t.cols as usize
            || point.line.0 + t.term.display_offset() as i32 >= t.rows as i32
        {
            return None;
        }
        let (text, cell_of) = mica_render::frame::row_string(t.term.grid(), point.line.0);
        let (column, cells) = span_of_token(&text, &cell_of, point.column.0)?;
        let row = &t.term.grid()[point.line];
        if (column..column + cells).any(|c| {
            row[Column(c)]
                .flags
                .contains(mica_core::surface::CellFlags::HIDDEN)
        }) {
            return None;
        }
        Some(PathHover {
            pane_id,
            line: point.line.0,
            column,
            cells,
        })
    })
}

/// 命中 token 的 (col_start, cell 宽):重扫一遍行找覆盖 cursor_col 的
/// token 边界(detect 判定已过,这里只取几何)
pub(super) fn span_of_token(
    text: &str,
    cell_of: &[usize],
    cursor_col: usize,
) -> Option<(usize, usize)> {
    all_path_spans(text, cell_of)
        .into_iter()
        .find(|(start, width)| (*start..*start + *width).contains(&cursor_col))
}

/// 客户区坐标处的 OSC 8 URI 或可点击路径(Ctrl+Click 与 hover 气泡共用;None = 无链接)。
pub(super) fn link_at(px: i32, py: i32) -> Option<String> {
    let (pane_id, r) = active_pane_rects().into_iter().find(|(_, r)| {
        (px as f32) >= r.x
            && (px as f32) < r.x + r.w
            && (py as f32) >= r.y
            && (py as f32) < r.y + r.h
    })?;
    TABS.with(|tabs| {
        let guard = tabs.borrow();
        let t = &guard
            .get(ACTIVE.get())?
            .panes
            .iter()
            .find(|p| p.id == pane_id)?
            .terminal;
        let (point, _) = px_to_buffer_point(
            px as f32 - r.x,
            py as f32 - r.y,
            &t.metrics,
            t.term.display_offset(),
        );
        if point.column.0 >= t.cols as usize
            || point.line.0 + t.term.display_offset() as i32 >= t.rows as i32
        {
            return None;
        }
        let (line, col) = (point.line.0, point.column.0);
        let cell = &t.term.grid()[Line(line)][Column(col)];
        if cell.flags.contains(mica_core::surface::CellFlags::HIDDEN) {
            return None;
        }
        if let Some(l) = cell.hyperlink() {
            return Some(l.uri().to_string());
        }
        // #58:OSC 8 未命中 → 行文本路径检测(编译器输出裸路径可点击)
        let (text, cell_of) = mica_render::frame::row_string(t.term.grid(), line);
        path_at_in_row(t, &text, &cell_of, col)
    })
}

/// 裸路径检测(#58)入口:命中 token 后相对路径用 OSC 7 cwd 补全
fn path_at_in_row(
    t: &Terminal,
    text: &str,
    cell_of: &[usize],
    cursor_col: usize,
) -> Option<String> {
    let path = detect_path_token(text, cell_of, cursor_col)?;
    Some(match path.chars().next() {
        Some('/') | Some('\\') => path,
        _ => {
            let c = path.as_str();
            if c.len() > 2 && c.as_bytes()[1] == b':' {
                path // 盘符绝对路径
            } else {
                match t.term.cwd() {
                    Some(cwd) => format!("{}\\{}", cwd.trim_end_matches('\\'), path),
                    None => path,
                }
            }
        }
    })
}

/// #58 纯检测(不碰 Terminal,可 TDD):token 化 → 模式判定 →
/// 定位剥离(:line[:col])→ 光标覆盖判定
pub(super) fn detect_path_token(
    text: &str,
    cell_of: &[usize],
    cursor_col: usize,
) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let is_word = |i: usize| {
        let c = bytes[i];
        c.is_alphanumeric() || matches!(c, '.' | '/' | '\\' | ':' | '_' | '-' | '+' | '@')
            // Combining characters belong to the preceding grid cell. Keep
            // their exact spelling instead of splitting a Unicode filename.
            || (i > 0 && cell_of.get(i).zip(cell_of.get(i-1)).is_some_and(|(a,b)| a == b))
    };
    let mut i = 0;
    while i < bytes.len() {
        if !is_word(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_word(i) {
            i += 1;
        }
        let token: String = bytes[start..i].iter().collect();
        if let Some(path) = normalize_path_token(&token) {
            // 光标落在 token 覆盖区(宽字符 token 的 cell 映射收缩)
            let first = cell_of[start];
            let end = cell_of
                .get(i)
                .copied()
                .unwrap_or_else(|| cell_of[i - 1] + 1);
            if (first..end).contains(&cursor_col) {
                return Some(path);
            }
        }
    }
    None
}

/// token → 文件系统路径:剥离尾部 :line[:col],按模式判定是否路径。
/// None = 不是路径(误报防线)
pub(super) fn normalize_path_token(token: &str) -> Option<String> {
    if token.contains("://") {
        return None;
    }
    // 剥离 :N[:N] 定位后缀(仅当剩余部分像路径)
    let mut path = token;
    let mut num = 0;
    while let Some(idx) = path.rfind(':') {
        let tail = &path[idx + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            path = &path[..idx];
            num += 1;
        } else {
            break;
        }
    }
    if path.is_empty() {
        return None;
    }
    let slashes = path.matches(['/']).count() + path.matches(['\\']).count();
    let looks_abs = {
        let b = path.as_bytes();
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
    };
    let rel_start = path.starts_with("./")
        || path.starts_with("../")
        || path.starts_with(".\\")
        || path.starts_with("..\\")
        || path.starts_with('/')
        || path.starts_with('\\');
    let has_ext = path.rsplit(['/', '\\']).next().is_some_and(|f| {
        let f = f.rsplit_once('.');
        f.is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.len() <= 8)
    });
    // 单斜杠无扩展名(a/b)太弱不吃;多斜杠(嵌套目录)放行
    let is_path = looks_abs || rel_start || has_ext || slashes >= 2;
    if !is_path || slashes == 0 && !looks_abs && !rel_start {
        return None; // 无任何分隔符的裸词
    }
    let _ = num;
    Some(path.replace('/', "\\"))
}
