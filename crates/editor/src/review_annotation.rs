//! Review annotations: a popup that inserts a `[TYPE] text` review marker into
//! the buffer as a comment line. The marker is plain text, so review comments
//! persist with the file, show in every view, and aggregate via project search.
//! Types come from the `review_annotation_types` setting (default `ISSUE`,
//! `SUGGESTION`, `NOTE`, `PRAISE`; first entry is initially selected).
//!
//! A marker is only written where it is valid source: a writable buffer, a row
//! that exists in the working copy, and a language that has comment syntax.
//! Every other case refuses with a notification and keeps whatever the reviewer
//! typed, because a review tool that silently discards a comment is worse than
//! one that declines to write it.

use super::*;

const DEFAULT_TYPES: [&str; 4] = ["ISSUE", "SUGGESTION", "NOTE", "PRAISE"];

struct ReviewAnnotationNotice;

/// The comment types offered by the popup, from `review_annotation_types`.
/// The first entry is initially selected; Tab cycles in list order.
fn configured_types(cx: &App) -> Vec<String> {
    normalized_types(&EditorSettings::get_global(cx).review_annotation_types)
}

/// A label is usable only when it survives round-tripping through `[LABEL]`
/// and through the shell tools that read the markers back. That means letters,
/// digits, underscore, and dash: a bracket would end the label early, and `:`
/// or `+` would collide with the `:+N` span suffix.
fn is_usable_type(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn normalized_types(configured: &[String]) -> Vec<String> {
    let mut types: Vec<String> = Vec::new();
    for label in configured {
        let label = label.trim().to_uppercase();
        if is_usable_type(&label) && !types.contains(&label) {
            types.push(label);
        }
    }
    if types.is_empty() {
        DEFAULT_TYPES
            .iter()
            .map(|label| label.to_string())
            .collect()
    } else {
        types
    }
}

fn type_color(label: &str, status: &theme::StatusColors) -> Hsla {
    match label {
        "NOTE" => status.hint,
        "SUGGESTION" => status.info,
        "ISSUE" => status.error,
        "PRAISE" => status.success,
        _ => status.info,
    }
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

fn row_text(snapshot: &MultiBufferSnapshot, row: u32) -> String {
    let start = Point::new(row, 0);
    let end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
    snapshot.text_for_range(start..end).collect()
}

/// The language at the row's first non-whitespace column. Column zero reports
/// the outer language inside a Markdown fence or an HTML `<script>`, which
/// would pick the wrong comment leader.
fn scope_for_row(snapshot: &MultiBufferSnapshot, row: u32, line: &str) -> Option<LanguageScope> {
    let column = leading_whitespace(line).len() as u32;
    snapshot.language_scope_at(Point::new(row, column))
}

fn has_comment_syntax(scope: Option<&LanguageScope>) -> bool {
    scope.is_some_and(|scope| {
        !scope.line_comment_prefixes().is_empty() || scope.block_comment().is_some()
    })
}

/// Which comment leader opened a row, which decides whether a trailing block
/// terminator belongs to the comment or to the text.
#[derive(PartialEq)]
enum Leader {
    Line,
    Block,
}

/// Strips the row's indentation and comment leader, leaving the commented text.
/// A row with no comment leader is not a comment, so it yields nothing: the
/// popup only ever writes markers behind a leader, and treating bare `[NOTE]`
/// prose as a marker would let the delete and strip paths eat ordinary lines.
fn strip_comment_leader<'a>(
    line: &'a str,
    scope: Option<&LanguageScope>,
) -> Option<(&'a str, Leader, &'a str)> {
    let trimmed = line.trim_start();
    let scope = scope?;
    // Longest first: a language listing both `// ` and `/// ` would otherwise
    // strip `// ` off a doc comment and leave a stray slash behind.
    let mut prefixes: Vec<&Arc<str>> = scope.line_comment_prefixes().iter().collect();
    prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.len()));
    for prefix in prefixes {
        if let Some(rest) = trimmed.strip_prefix(prefix.as_ref()) {
            let leader = &trimmed[..prefix.len()];
            return Some((rest.trim_start(), Leader::Line, leader));
        }
    }
    if let Some(block) = scope.block_comment()
        && let Some(rest) = trimmed.strip_prefix(block.start.as_ref())
    {
        let leader = &trimmed[..block.start.len()];
        return Some((rest.trim_start(), Leader::Block, leader));
    }
    None
}

/// A marker recovered from a row, for editing, deleting, and navigating.
struct ParsedAnnotation {
    type_label: String,
    /// Extra rows the comment covers beyond the annotated line, from the
    /// `:+N` suffix. Zero for a single-line marker.
    span_rows: u32,
    text: String,
    /// The exact leader the row already uses, so rewriting a marker keeps the
    /// comment style the reviewer wrote it in rather than normalizing a doc
    /// comment down to a plain one.
    leader: String,
}

/// Renders the bracketed part of a marker. A multi-row comment carries its
/// span as `:+N` relative rows, which survives edits above it in a way an
/// absolute line range would not.
fn marker_label(type_label: &str, span_rows: u32) -> String {
    if span_rows == 0 {
        format!("[{type_label}]")
    } else {
        format!("[{type_label}:+{span_rows}]")
    }
}

/// Splits `ISSUE:+3` into its type and span. A malformed span makes the whole
/// label unrecognized rather than silently reading as a single-line marker.
fn split_type_and_span(label: &str) -> Option<(&str, u32)> {
    match label.split_once(":+") {
        Some((type_label, span)) => Some((type_label, span.parse().ok()?)),
        None => Some((label, 0)),
    }
}

/// Parses a row that is *entirely* a review marker. The marker must be the
/// first token after the comment leader, so prose mentioning `[ISSUE]` mid
/// sentence is not a marker.
fn parse_annotation_line(
    line: &str,
    scope: Option<&LanguageScope>,
    types: &[String],
) -> Option<ParsedAnnotation> {
    let (rest, leader_kind, leader) = strip_comment_leader(line, scope)?;
    let rest = rest.strip_prefix('[')?;
    let (label, rest) = rest.split_once(']')?;
    let (type_label, span_rows) = split_type_and_span(label)?;
    if !types.iter().any(|known| known == type_label) {
        return None;
    }
    // The bracket must be a whole token. Without this, an ordinary comment
    // reading `// [NOTE]worthy behavior` would be deletable as an annotation.
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let mut text = rest.trim();
    // A block comment must close on the same line. `/* [ISSUE] x */ call();`
    // is a line of code carrying a comment, not an annotation, and treating it
    // as one would let the delete and strip paths remove the call. In a line
    // comment a trailing `*/` is instead part of what the reviewer wrote.
    if leader_kind == Leader::Block {
        let block = scope.and_then(|scope| scope.block_comment())?;
        text = text.strip_suffix(block.end.as_ref())?.trim_end();
        // The comment must also be the only one on the row. `/* [ISSUE] x */
        // call(); /* t */` ends in a terminator but is still a line of code,
        // and deleting it would take the call with it.
        if text.contains(block.end.as_ref()) {
            return None;
        }
    }
    Some(ParsedAnnotation {
        type_label: type_label.to_string(),
        span_rows,
        text: text.to_string(),
        leader: leader.to_string(),
    })
}

/// Where a marker will go. Resolved before the popup opens so that every
/// refusal happens while the comment is still unwritten.
struct AnnotationTarget {
    row: u32,
    span_rows: u32,
    existing: Option<ParsedAnnotation>,
}

/// The active annotation input popup. At most one per editor.
pub struct ReviewAnnotationPopup {
    block_id: CustomBlockId,
    prompt_editor: Entity<Editor>,
    /// Tracks the start of the annotated row across concurrent edits.
    anchor: Anchor,
    types: Vec<String>,
    type_index: usize,
    /// Extra rows the comment covers beyond the annotated line.
    span_rows: u32,
    _subscriptions: Vec<Subscription>,
}

impl ReviewAnnotationPopup {
    fn selected_type(&self) -> &str {
        &self.types[self.type_index]
    }
}

impl Editor {
    fn notify_review_annotation(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        let message = message.into();
        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<ReviewAnnotationNotice>(), message),
                cx,
            );
        });
    }

    fn cursor_row(&mut self, cx: &mut Context<Self>) -> u32 {
        self.selected_rows(cx).0
    }

    /// The first and last buffer row the newest selection touches. A selection
    /// ending at column zero stops on the previous row, so dragging onto the
    /// next line does not silently widen the comment.
    fn selected_rows(&mut self, cx: &mut Context<Self>) -> (u32, u32) {
        let display_snapshot = self.display_snapshot(cx);
        let selection = self.selections.newest_display(&display_snapshot);
        let start = display_snapshot
            .display_point_to_point(DisplayPoint::new(selection.start.row(), 0), Bias::Left)
            .row;
        let end_point = display_snapshot.display_point_to_point(selection.end, Bias::Left);
        let end = if end_point.column == 0 && end_point.row > start {
            end_point.row - 1
        } else {
            end_point.row
        };
        (start, end.max(start))
    }

    /// True when the row is base text shown by an expanded diff hunk. Such a
    /// row has no working-copy position, and `MultiBuffer::edit` drops edits
    /// aimed at it without reporting anything.
    ///
    /// The query is the row's start alone. Asking about the whole row would
    /// also return a region that merely begins where the row ends, because the
    /// iterator stops only once a region starts past the end of the range, and
    /// a live row sitting immediately before a deletion would be refused.
    fn row_is_deleted(&self, snapshot: &MultiBufferSnapshot, row: u32) -> bool {
        let start = Point::new(row, 0);
        snapshot
            .range_to_buffer_ranges_with_deleted_hunks(start..start)
            .next()
            .is_some_and(|(_, _, deleted_hunk_anchor)| deleted_hunk_anchor.is_some())
    }

    /// Rows the comment covers beyond its first, counted in working-copy rows.
    /// Base rows shown between the selected lines occupy multibuffer rows but
    /// no file lines, so counting raw rows would export a range covering lines
    /// the reviewer never selected.
    fn live_span_rows(&self, snapshot: &MultiBufferSnapshot, first: u32, last: u32) -> u32 {
        (first..=last)
            .filter(|row| !self.row_is_deleted(snapshot, *row))
            .count()
            .saturating_sub(1) as u32
    }

    fn resolve_annotation_target(&mut self, cx: &mut Context<Self>) -> Option<AnnotationTarget> {
        if self.read_only(cx) {
            self.notify_review_annotation(
                "Review annotations need a writable buffer; this view is read-only.",
                cx,
            );
            return None;
        }

        let (row, last_row) = self.selected_rows(cx);
        let snapshot = self.buffer.read(cx).snapshot(cx);

        if self.row_is_deleted(&snapshot, row) {
            self.notify_review_annotation(
                "Cannot annotate a deleted line; move to a line that exists in the working copy.",
                cx,
            );
            return None;
        }

        let line = row_text(&snapshot, row);
        let scope = scope_for_row(&snapshot, row, &line);
        if !has_comment_syntax(scope.as_ref()) {
            self.notify_review_annotation(
                "This language has no comment syntax, so a review marker would not be valid here.",
                cx,
            );
            return None;
        }

        let existing = parse_annotation_line(&line, scope.as_ref(), &configured_types(cx));
        // Editing a marker keeps the span it already records; a fresh marker
        // takes the span from the selection.
        let span_rows = match &existing {
            Some(existing) => existing.span_rows,
            None => self.live_span_rows(&snapshot, row, last_row),
        };
        Some(AnnotationTarget {
            row,
            span_rows,
            existing,
        })
    }

    /// Opens the review annotation popup on the newest selection's start row,
    /// or focuses the existing popup. A row that already holds a marker is
    /// edited in place.
    pub(crate) fn insert_review_annotation(
        &mut self,
        _: &InsertReviewAnnotation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(popup) = &self.review_annotation_popup {
            let focus_handle = popup.prompt_editor.focus_handle(cx);
            window.focus(&focus_handle, cx);
            return;
        }

        let Some(target) = self.resolve_annotation_target(cx) else {
            return;
        };

        let types = configured_types(cx);
        let type_index = target
            .existing
            .as_ref()
            .and_then(|existing| types.iter().position(|known| *known == existing.type_label))
            .unwrap_or(0);
        let existing_text = target
            .existing
            .as_ref()
            .map(|existing| existing.text.clone());

        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let anchor = buffer_snapshot.anchor_before(Point::new(target.row, 0));
        let line_len = buffer_snapshot.line_len(MultiBufferRow(target.row));
        let block_anchor = buffer_snapshot.anchor_after(Point::new(target.row, line_len));

        let prompt_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Add a review comment…", window, cx);
            if let Some(text) = existing_text {
                editor.set_text(text, window, cx);
            }
            editor
        });

        let parent_editor = cx.entity().downgrade();
        let subscriptions = prompt_editor.update(cx, |prompt_editor, _cx| {
            vec![
                prompt_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &crate::actions::Newline, window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.submit_review_annotation(window, cx);
                            });
                        }
                    }
                }),
                prompt_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &crate::actions::Tab, _window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.cycle_review_annotation_kind(false, cx);
                            });
                        }
                    }
                }),
                prompt_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &crate::actions::Backtab, _window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.cycle_review_annotation_kind(true, cx);
                            });
                        }
                    }
                }),
                prompt_editor.register_action({
                    let parent_editor = parent_editor.clone();
                    move |_: &crate::actions::Cancel, window, cx| {
                        if let Some(editor) = parent_editor.upgrade() {
                            editor.update(cx, |editor, cx| {
                                editor.dismiss_review_annotation_popup(window, cx);
                            });
                        }
                    }
                }),
            ]
        });

        let prompt_for_render = prompt_editor.clone();
        let editor_handle = cx.entity().downgrade();
        let block = BlockProperties {
            style: BlockStyle::Sticky,
            placement: BlockPlacement::Below(block_anchor),
            height: Some(2),
            render: Arc::new(move |cx| {
                render_review_annotation_popup(&prompt_for_render, &editor_handle, cx)
            }),
            priority: 0,
        };

        let Some(block_id) = self.insert_blocks([block], None, cx).into_iter().next() else {
            log::error!("failed to insert review annotation block");
            return;
        };

        self.review_annotation_popup = Some(ReviewAnnotationPopup {
            block_id,
            prompt_editor: prompt_editor.clone(),
            anchor,
            types,
            type_index,
            span_rows: target.span_rows,
            _subscriptions: subscriptions,
        });

        let focus_handle = prompt_editor.focus_handle(cx);
        window.focus(&focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn cycle_review_annotation_kind(&mut self, reverse: bool, cx: &mut Context<Self>) {
        if let Some(popup) = &mut self.review_annotation_popup {
            let len = popup.types.len();
            let step = if reverse { len - 1 } else { 1 };
            popup.type_index = (popup.type_index + step) % len;
            cx.notify();
        }
    }

    pub(crate) fn dismiss_review_annotation_popup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(popup) = self.review_annotation_popup.take() {
            self.remove_blocks(HashSet::from_iter([popup.block_id]), None, cx);
            let focus_handle = self.focus_handle.clone();
            window.focus(&focus_handle, cx);
            cx.notify();
        }
    }

    /// Writes the marker and closes the popup. A refusal leaves the popup open
    /// so the typed comment survives.
    pub(crate) fn submit_review_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(popup) = &self.review_annotation_popup else {
            return;
        };
        let text = popup
            .prompt_editor
            .read(cx)
            .text(cx)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            self.dismiss_review_annotation_popup(window, cx);
            return;
        }
        let label = marker_label(popup.selected_type(), popup.span_rows);
        let anchor = popup.anchor;

        if self.read_only(cx) {
            self.notify_review_annotation(
                "Review annotations need a writable buffer; this view is read-only.",
                cx,
            );
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);

        // A removed excerpt (a diff refresh, a file closed in a multibuffer)
        // leaves the anchor resolving to a neighbouring position rather than
        // failing, which would put the marker on an unrelated line.
        if !anchor.is_valid(&snapshot) {
            self.notify_review_annotation(
                "The annotated line is no longer open; reopen the file and add the comment again.",
                cx,
            );
            return;
        }

        let row = anchor.to_point(&snapshot).row;

        if self.row_is_deleted(&snapshot, row) {
            self.notify_review_annotation(
                "Cannot annotate a deleted line; move to a line that exists in the working copy.",
                cx,
            );
            return;
        }

        let line = row_text(&snapshot, row);
        let indent = leading_whitespace(&line).to_string();
        let scope = scope_for_row(&snapshot, row, &line);
        // A block comment ends at the first terminator, so text containing one
        // would close the comment early and push the rest into the source.
        if let Some(scope) = scope.as_ref()
            && scope.line_comment_prefixes().is_empty()
            && let Some(block) = scope.block_comment()
            && text.contains(block.end.as_ref())
        {
            self.notify_review_annotation(
                format!(
                    "A review comment here cannot contain `{}`, which would close the comment early.",
                    block.end
                ),
                cx,
            );
            return;
        }

        // Whether this replaces the row is decided from the row as it stands
        // now, not from what it held when the popup opened. If the marker was
        // deleted meanwhile, the anchor has slid onto ordinary code, and
        // trusting the old answer would overwrite it.
        let existing = parse_annotation_line(&line, scope.as_ref(), &configured_types(cx));
        let replaces_row = existing.is_some();

        // Rewriting a marker keeps the leader already on the row, so editing a
        // doc comment does not quietly demote it to a plain one.
        let existing_line_leader = existing.as_ref().map(|existing| existing.leader.clone());

        let Some(marker) = scope.as_ref().and_then(|scope| {
            let line_leader = existing_line_leader
                .filter(|leader| {
                    scope
                        .line_comment_prefixes()
                        .iter()
                        .any(|prefix| prefix.as_ref() == leader)
                })
                .or_else(|| {
                    scope
                        .line_comment_prefixes()
                        .first()
                        .map(|prefix| prefix.to_string())
                });
            if let Some(leader) = line_leader {
                Some(format!("{indent}{leader}{label} {text}"))
            } else {
                scope
                    .block_comment()
                    .map(|block| format!("{indent}{} {label} {text} {}", block.start, block.end))
            }
        }) else {
            self.notify_review_annotation(
                "This language has no comment syntax, so a review marker would not be valid here.",
                cx,
            );
            return;
        };

        let (range, replacement) = if replaces_row {
            (
                Point::new(row, 0)..Point::new(row, snapshot.line_len(MultiBufferRow(row))),
                marker,
            )
        } else {
            (
                Point::new(row, 0)..Point::new(row, 0),
                format!("{marker}\n"),
            )
        };

        self.transact(window, cx, |this, _window, cx| {
            this.buffer.update(cx, |buffer, cx| {
                buffer.edit([(range, replacement)], None, cx);
            });
        });

        self.dismiss_review_annotation_popup(window, cx);
    }

    pub(crate) fn go_to_next_review_annotation(
        &mut self,
        _: &GoToNextReviewAnnotation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_review_annotation(true, window, cx);
    }

    pub(crate) fn go_to_previous_review_annotation(
        &mut self,
        _: &GoToPreviousReviewAnnotation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_to_review_annotation(false, window, cx);
    }

    fn move_to_review_annotation(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.cursor_row(cx);
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let types = configured_types(cx);
        let max_row = snapshot.max_point().row;

        // Lazily, and stopping at the first hit, so a large buffer costs only
        // the rows between the cursor and the next marker.
        let mut candidates: Box<dyn Iterator<Item = u32>> = if forward {
            Box::new(row.saturating_add(1)..=max_row)
        } else {
            Box::new((0..row).rev())
        };

        // Base text shown by an expanded hunk can hold a marker that no longer
        // exists in the working copy; landing on one offers an edit that could
        // not be written.
        let found = candidates.find(|candidate| {
            if self.row_is_deleted(&snapshot, *candidate) {
                return false;
            }
            let line = row_text(&snapshot, *candidate);
            let scope = scope_for_row(&snapshot, *candidate, &line);
            parse_annotation_line(&line, scope.as_ref(), &types).is_some()
        });

        if let Some(candidate) = found {
            let destination = Point::new(candidate, 0);
            self.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |s| s.select_ranges([destination..destination]),
            );
            return;
        }

        self.notify_review_annotation(
            if forward {
                "No review annotation below the cursor."
            } else {
                "No review annotation above the cursor."
            },
            cx,
        );
    }

    /// Deletes the marker on the cursor's row, whole line, in one transaction.
    pub(crate) fn delete_review_annotation(
        &mut self,
        _: &DeleteReviewAnnotation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            self.notify_review_annotation(
                "Review annotations need a writable buffer; this view is read-only.",
                cx,
            );
            return;
        }

        let row = self.cursor_row(cx);
        let snapshot = self.buffer.read(cx).snapshot(cx);

        if self.row_is_deleted(&snapshot, row) {
            self.notify_review_annotation(
                "This line is deleted base text, not part of the working copy.",
                cx,
            );
            return;
        }

        let line = row_text(&snapshot, row);
        let scope = scope_for_row(&snapshot, row, &line);
        if parse_annotation_line(&line, scope.as_ref(), &configured_types(cx)).is_none() {
            self.notify_review_annotation("This line is not a review annotation.", cx);
            return;
        }

        let max_row = snapshot.max_point().row;
        let end = if row < max_row {
            Point::new(row + 1, 0)
        } else {
            Point::new(row, snapshot.line_len(MultiBufferRow(row)))
        };

        self.transact(window, cx, |this, _window, cx| {
            this.buffer.update(cx, |buffer, cx| {
                buffer.edit([(Point::new(row, 0)..end, "")], None, cx);
            });
        });
    }

    #[cfg(test)]
    pub(crate) fn review_annotation_prompt_editor(&self) -> Option<&Entity<Editor>> {
        self.review_annotation_popup
            .as_ref()
            .map(|popup| &popup.prompt_editor)
    }
}

fn render_review_annotation_popup(
    prompt_editor: &Entity<Editor>,
    editor_handle: &WeakEntity<Editor>,
    cx: &mut BlockContext,
) -> AnyElement {
    let theme = cx.theme();
    let colors = theme.colors();
    let status = theme.status();

    let type_label = editor_handle
        .upgrade()
        .and_then(|editor| {
            editor
                .read(cx)
                .review_annotation_popup
                .as_ref()
                .map(|popup| popup.selected_type().to_string())
        })
        .unwrap_or_else(|| DEFAULT_TYPES[0].to_string());
    let kind_color = type_color(&type_label, status);

    let badge = {
        let editor_handle = editor_handle.clone();
        h_flex()
            .id("review-annotation-kind")
            .flex_shrink_0()
            .px_1()
            .rounded_sm()
            .cursor_pointer()
            .bg(kind_color.opacity(0.12))
            .child(
                Label::new(type_label)
                    .size(LabelSize::XSmall)
                    .color(Color::Custom(kind_color)),
            )
            .tooltip(Tooltip::text("Change comment type (Tab)"))
            .on_click(move |_, _window, cx| {
                if let Some(editor) = editor_handle.upgrade() {
                    editor.update(cx, |editor, cx| {
                        editor.cycle_review_annotation_kind(false, cx);
                    });
                }
            })
    };

    v_flex()
        .w_full()
        .bg(colors.editor_background)
        .border_b_1()
        .border_color(colors.border)
        .px_2()
        .py_1p5()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .gap_2()
                .rounded_md()
                .bg(colors.surface_background)
                .px_2()
                .py_1()
                .child(badge)
                .child(
                    div()
                        .flex_1()
                        .border_1()
                        .border_color(colors.border)
                        .rounded_md()
                        .bg(colors.editor_background)
                        .px_2()
                        .py_1()
                        .child(prompt_editor.clone()),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types() -> Vec<String> {
        DEFAULT_TYPES
            .iter()
            .map(|label| label.to_string())
            .collect()
    }

    #[test]
    fn normalized_types_uppercases_and_drops_blanks() {
        let configured = vec![
            "blocker".to_string(),
            "  question ".to_string(),
            "".to_string(),
            "   ".to_string(),
        ];
        assert_eq!(
            normalized_types(&configured),
            vec!["BLOCKER".to_string(), "QUESTION".to_string()]
        );
    }

    #[test]
    fn normalized_types_deduplicates() {
        let configured = vec![
            "issue".to_string(),
            "ISSUE ".to_string(),
            "note".to_string(),
        ];
        assert_eq!(
            normalized_types(&configured),
            vec!["ISSUE".to_string(), "NOTE".to_string()]
        );
    }

    #[test]
    fn normalized_types_rejects_labels_that_break_the_marker() {
        let configured = vec![
            "two words".to_string(),
            "brack]et".to_string(),
            "good".to_string(),
        ];
        assert_eq!(normalized_types(&configured), vec!["GOOD".to_string()]);
    }

    #[test]
    fn normalized_types_falls_back_to_defaults_when_empty() {
        assert_eq!(normalized_types(&[]), types());
        assert_eq!(normalized_types(&["  ".to_string()]).len(), 4);
    }

    #[test]
    fn normalized_types_rejects_labels_that_collide_with_the_span_suffix() {
        let configured = vec!["a:+2".to_string(), "b+".to_string(), "good".to_string()];
        assert_eq!(normalized_types(&configured), vec!["GOOD".to_string()]);
    }

    /// Without a language there is no comment leader to find, and a bare
    /// bracketed line is ordinary prose. Marker recognition against real
    /// languages is covered by the editor tests.
    #[test]
    fn does_not_parse_a_line_with_no_comment_leader() {
        assert!(parse_annotation_line("[ISSUE] this can panic", None, &types()).is_none());
        assert!(
            parse_annotation_line("see the [ISSUE] tracker for context", None, &types()).is_none()
        );
    }

    #[test]
    fn renders_a_span_only_for_multi_row_comments() {
        assert_eq!(marker_label("ISSUE", 0), "[ISSUE]");
        assert_eq!(marker_label("ISSUE", 3), "[ISSUE:+3]");
    }

    #[test]
    fn splits_a_span_suffix_from_the_type() {
        assert_eq!(split_type_and_span("ISSUE"), Some(("ISSUE", 0)));
        assert_eq!(split_type_and_span("ISSUE:+3"), Some(("ISSUE", 3)));
    }

    #[test]
    fn rejects_a_malformed_span() {
        assert_eq!(split_type_and_span("ISSUE:+x"), None);
        assert_eq!(split_type_and_span("ISSUE:+"), None);
    }
}
