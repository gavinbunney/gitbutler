//! A view of Git's index, alongside the usual uncommitted-change composer.

use std::collections::{BTreeSet, HashSet};

use gpui::{
    CursorStyle, DragMoveEvent, EmptyView, MouseButton, UniformListScrollHandle, list, uniform_list,
};

use super::*;
use crate::backend::staging::{self, Side as IndexSide};
use crate::state::Overlay;
use crate::ui::diff::{HeaderDecor, Layout, LineDecor};
use crate::ui::file_icons::file_icon;
use crate::ui::scrollbar::{Scroll, scrolled};
use crate::ui::{list_row, segmented};

#[derive(Clone, Copy)]
enum Resize {
    Lists,
    Composer,
}

#[derive(Clone)]
struct DragFiles {
    project: ProjectId,
    side: IndexSide,
    tree: gix::ObjectId,
    picks: Vec<staging::Pick>,
}

struct FileDragPreview(SharedString);

impl gpui::Render for FileDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::popover(Theme::get(cx))
            .px(dp(12.))
            .py(dp(8.))
            .child(self.0.clone())
    }
}

#[derive(Clone, Copy)]
enum Row {
    File,
    Hunk(usize),
    Line(usize, usize),
    Note,
}

pub(super) struct StagingState {
    pub(super) shown: Option<bool>,
    status: Option<Arc<staging::Status>>,
    selected: Option<(IndexSide, String)>,
    fingerprint: Option<u64>,
    picks: HashSet<(HunkKey, usize)>,
    kept: HashSet<(HunkKey, usize)>,
    anchor: Option<(HunkKey, usize)>,
    rows: Vec<Row>,
    diff: ListState,
    unstaged_scroll: UniformListScrollHandle,
    staged_scroll: UniformListScrollHandle,
    composer_height: f32,
    lists_share: f32,
    loading: bool,
    queued: bool,
    error: Option<SharedString>,
    _load: Task<()>,
}

impl Default for StagingState {
    fn default() -> Self {
        Self {
            shown: None,
            status: None,
            selected: None,
            fingerprint: None,
            picks: HashSet::new(),
            kept: HashSet::new(),
            anchor: None,
            rows: Vec::new(),
            diff: ListState::new(0, ListAlignment::Top, fixed(400.)),
            unstaged_scroll: UniformListScrollHandle::new(),
            staged_scroll: UniformListScrollHandle::new(),
            composer_height: 200.,
            lists_share: 0.5,
            loading: false,
            queued: false,
            error: None,
            _load: Task::ready(()),
        }
    }
}

impl StagingState {
    fn ready(&self) -> bool {
        !self.loading
            && self.error.is_none()
            && self
                .status
                .as_ref()
                .is_some_and(|status| status.index_tree.is_some())
    }

    pub(super) fn remeasure(&self) {
        self.diff.remeasure();
    }

    pub(super) fn suspend(&mut self) {
        self._load = Task::ready(());
        self.loading = false;
        self.queued = false;
    }

    fn file(&self) -> Option<&FileDiff> {
        let (side, path) = self.selected.as_ref()?;
        self.status
            .as_ref()?
            .files(*side)
            .iter()
            .find(|f| f.file.path == *path)
    }

    fn settle(&mut self) {
        if self.file().is_none() {
            self.fingerprint = None;
            self.selected = self.status.as_ref().and_then(|status| {
                [IndexSide::Unstaged, IndexSide::Staged]
                    .into_iter()
                    .find_map(|side| Some((side, status.files(side).first()?.file.path.clone())))
            });
        }
        let fingerprint = self.file().map(|file| file.fingerprint);
        if self.fingerprint != fingerprint {
            self.fingerprint = fingerprint;
            self.picks.clear();
            self.kept.clear();
            self.anchor = None;
        }
        self.rows = self.file().map_or_else(Vec::new, |file| {
            let mut rows = vec![Row::File];
            if file.hunks().is_empty() {
                rows.push(Row::Note);
            } else {
                for (h, hunk) in file.hunks().iter().enumerate() {
                    rows.push(Row::Hunk(h));
                    rows.extend((0..hunk.lines.len()).map(|line| Row::Line(h, line)));
                }
            }
            rows
        });
        self.diff.reset(self.rows.len());
    }
}

impl Detail {
    fn staging(&self) -> Option<&StagingState> {
        match &self.page {
            page::PageState::Changes { staging, .. } => Some(staging),
            _ => None,
        }
    }

    fn staging_mut(&mut self) -> Option<&mut StagingState> {
        match &mut self.page {
            page::PageState::Changes { staging, .. } => Some(staging),
            _ => None,
        }
    }

    pub(super) fn staging_available(&self, cx: &App) -> bool {
        matches!(self.selection(), Some(Selection::Uncommitted(ref project, Checkout::Main)) if self.editing(project, cx).is_none())
    }

    pub(super) fn staging_shown(&self, cx: &App) -> bool {
        self.staging_available(cx)
            && self
                .staging()
                .and_then(|state| state.shown)
                .unwrap_or(self.store.read(cx).prefs.staging_view)
    }

    pub(super) fn render_staging_tabs(
        &self,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let on = self.staging_shown(cx);
        segmented(
            true,
            theme,
            [
                ("changes-view", "Changes", false),
                ("staging-view", "Staging", true),
            ]
            .map(|(id, label, staging)| {
                (
                    div()
                        .id(id)
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(state) = this.staging_mut() {
                                state.shown = Some(staging);
                                state.suspend();
                            }
                            this.store.update(cx, |store, cx| {
                                store.set_prefs(|prefs| prefs.staging_view = staging, cx)
                            });
                            this.reset_view();
                            this.reload_page(true, cx);
                            this.refresh_diff(true, cx);
                            cx.notify();
                        })),
                    on == staging,
                )
            }),
        )
    }

    pub(super) fn load_staging(&mut self, cx: &mut Context<Self>) {
        let Some((project, Checkout::Main)) = self.selected_checkout() else {
            return;
        };
        let Some(path) = self.store.read(cx).path(&project) else {
            return;
        };
        let scope = self.store.read(cx).prefs.scope_lines();
        let Some(state) = self.staging_mut() else {
            return;
        };
        if state.loading {
            state.queued = true;
            return;
        }
        state.loading = true;
        let previous = state.status.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = backend::run(move || staging::load(&path, previous, scope)).await;
            this.update(cx, |this, cx| {
                if this.selected_project().as_ref() != Some(&project) {
                    return;
                }
                let Some(state) = this.staging_mut() else {
                    return;
                };
                state.loading = false;
                match result {
                    Ok(status) => {
                        state.status = Some(Arc::new(status));
                        state.error = None;
                        state.settle();
                    }
                    Err(err) => state.error = Some(format!("{err:#}").into()),
                }
                let again = std::mem::take(&mut state.queued);
                if again && this.staging_shown(cx) {
                    this.load_staging(cx);
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(state) = self.staging_mut() {
            state._load = task;
        }
    }

    fn index_drag(&self, side: IndexSide, paths: Option<&str>) -> Option<DragFiles> {
        let state = self.staging()?;
        if state.loading || state.error.is_some() {
            return None;
        }
        let status = state.status.as_ref()?;
        let picks = status
            .files(side)
            .iter()
            .filter(|file| paths.is_none_or(|path| file.file.path == path))
            .map(|file| staging::Pick {
                changes: Selected::whole(&file.file),
                fingerprint: file.fingerprint,
            })
            .collect::<Vec<_>>();
        if picks.is_empty() {
            return None;
        }
        Some(DragFiles {
            project: self.selected_project()?,
            side,
            tree: status.index_tree?,
            picks,
        })
    }

    fn change_index(&mut self, drag: DragFiles, cx: &mut Context<Self>) {
        if self.selected_project().as_ref() != Some(&drag.project) {
            return;
        }
        let DragFiles {
            project,
            side,
            tree,
            picks,
        } = drag;
        self.store.update(cx, |store, cx| {
            store.apply(
                &project,
                side.action(),
                Overlay::default(),
                staging::ChangeIndex {
                    side,
                    picks,
                    index_tree: tree,
                },
                |_, (), _| {},
                cx,
            );
        });
    }

    fn index_part(&mut self, key: HunkKey, discard: bool, cx: &mut Context<Self>) {
        let Some(state) = self.staging() else { return };
        let Some(hunk) = state
            .file()
            .and_then(|file| file.hunks().iter().find(|hunk| hunk.key == key))
        else {
            return;
        };
        let lines = state
            .picks
            .iter()
            .filter_map(|(h, line)| (*h == key).then_some(*line))
            .collect::<BTreeSet<_>>();
        let hunks = if lines.is_empty() {
            vec![key]
        } else {
            hunk.picked(&lines)
        };
        let Some((side, path)) = &state.selected else {
            return;
        };
        if discard && *side != IndexSide::Unstaged {
            return;
        }
        let Some(mut drag) = self.index_drag(*side, Some(path)) else {
            return;
        };
        if hunks.is_empty() {
            return;
        }
        if let Some(pick) = drag.picks.first_mut() {
            pick.changes.hunks = Some(hunks);
        }
        if discard {
            let Some(pick) = drag.picks.pop() else { return };
            self.store.update(cx, |store, cx| {
                store.apply(
                    &drag.project,
                    "Discard",
                    Overlay::default(),
                    staging::DiscardUnstaged {
                        pick,
                        index_tree: drag.tree,
                    },
                    |store, error, cx| match error {
                        None => store.snack_undoable("Changes discarded", cx),
                        Some(error) => store.error(
                            "Discard failed",
                            format!("{error}\nUndo restores the discarded changes."),
                            cx,
                        ),
                    },
                    cx,
                );
            });
        } else {
            self.change_index(drag, cx);
        }
    }

    fn pick_index_line(
        &mut self,
        hunk: HunkKey,
        line: usize,
        pick: lines::Pick,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.staging_mut() else {
            return;
        };
        let Some(file) = state.file() else { return };
        let changed = file
            .hunks()
            .iter()
            .flat_map(|h| {
                h.lines.iter().enumerate().filter_map(move |(l, value)| {
                    (value.kind != LineKind::Context).then_some((h.key, l))
                })
            })
            .collect::<Vec<_>>();
        let at = (hunk, line);
        if !changed.contains(&at) {
            return;
        }
        match pick {
            lines::Pick::Only => {
                state.picks = HashSet::from([at]);
                state.kept.clear();
                state.anchor = Some(at);
            }
            lines::Pick::Toggle => {
                if !state.picks.remove(&at) {
                    state.picks.insert(at);
                }
                state.kept = state.picks.clone();
                state.anchor = Some(at);
            }
            lines::Pick::Run => {
                let start = state
                    .anchor
                    .and_then(|anchor| changed.iter().position(|at| *at == anchor));
                let end = changed.iter().position(|value| *value == at);
                if let Some((start, end)) = start.zip(end) {
                    state.picks = state.kept.clone();
                    state.picks.extend(
                        changed
                            .iter()
                            .skip(start.min(end))
                            .take(start.abs_diff(end) + 1)
                            .copied(),
                    );
                }
            }
        }
        cx.notify();
    }

    pub(super) fn commit_index(&mut self, cx: &mut Context<Self>) {
        let Some((project, Checkout::Main)) = self.selected_checkout() else {
            return;
        };
        let Some(state) = self.staging() else { return };
        if state.loading || state.error.is_some() {
            return;
        }
        let Some(status) = &state.status else { return };
        let Some(index_tree) = status.index_tree else {
            return;
        };
        let target = self.effective_target(&project, cx);
        if self.target_went(&project, &target, cx) {
            return;
        }
        let Some(target) = target else {
            return;
        };
        let message = self.message.read(cx).text(cx);
        let written = message.clone();
        let detail = cx.entity().downgrade();
        let from = project.clone();
        let finished = project.clone();
        self.store.update(cx, |store, cx| {
            let intent = staging::CommitIndex {
                target,
                index_tree,
                message,
                run_hooks: store.runs_hooks(&project),
            };
            store.apply(
                &project,
                "Commit staged changes",
                Overlay::default(),
                intent,
                move |store, hook_error, cx| {
                    let draft = (from, Checkout::Main);
                    if store
                        .drafts
                        .get(&draft)
                        .is_some_and(|value| value.trim() == written.trim())
                    {
                        store.drafts.remove(&draft);
                    }
                    store.snack_undoable("Committed staged changes", cx);
                    if let Some(error) = hook_error {
                        store.snack(error, cx);
                    }
                    cx.spawn(async move |_, cx| {
                        detail
                            .update(cx, |this, cx| {
                                if this.selected_checkout()
                                    == Some((finished.clone(), Checkout::Main))
                                    && this.message.read(cx).holds(&written, cx)
                                {
                                    this.message
                                        .update(cx, |editor, cx| editor.set_text("", cx));
                                }
                                cx.notify();
                            })
                            .ok();
                    })
                    .detach();
                },
                cx,
            );
        });
    }

    fn render_index_list(
        &self,
        project: &ProjectId,
        side: IndexSide,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        let count = state.status.as_ref().map_or(0, |s| s.files(side).len());
        let all = self.index_drag(side, None);
        let scroll = match side {
            IndexSide::Unstaged => &state.unstaged_scroll,
            IndexSide::Staged => &state.staged_scroll,
        };
        let title = match side {
            IndexSide::Unstaged => "Unstaged files",
            IndexSide::Staged => "Staged files",
        };
        let project = project.clone();
        v_flex()
            .id(match side {
                IndexSide::Unstaged => "unstaged-list",
                IndexSide::Staged => "staged-list",
            })
            .size_full()
            .min_h_0()
            .bg(theme.bg_1)
            .drag_over::<DragFiles>(|style, _, _, cx| style.bg(Theme::get(cx).hover))
            .can_drop(move |drag, _, _| {
                drag.downcast_ref::<DragFiles>()
                    .is_some_and(|drag| drag.side != side && drag.project == project)
            })
            .on_drop(cx.listener(move |this, drag: &DragFiles, _, cx| {
                if drag.side != side {
                    this.change_index(drag.clone(), cx);
                }
            }))
            .child(
                h_flex()
                    .h(dp(42.))
                    .flex_none()
                    .px(dp(12.))
                    .gap(dp(6.))
                    .text_size(dp(12.))
                    .child(format!("{title} ({count})"))
                    .child(div().flex_1())
                    .child(
                        button(
                            match side {
                                IndexSide::Unstaged => "stage-all",
                                IndexSide::Staged => "unstage-all",
                            },
                            format!("{} all", side.action()),
                        )
                        .small()
                        .style(ButtonStyle::Outline)
                        .disabled_because(all.is_none().then_some("No changes available"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(drag) = all.clone() {
                                this.change_index(drag, cx);
                            }
                        })),
                    ),
            )
            .when(count == 0, |el| {
                el.child(
                    div()
                        .p(dp(14.))
                        .text_size(dp(12.))
                        .text_color(theme.text_3)
                        .child("Drop files here"),
                )
            })
            .when(count > 0, |el| {
                el.child(scrolled(
                    uniform_list(
                        match side {
                            IndexSide::Unstaged => "unstaged-files",
                            IndexSide::Staged => "staged-files",
                        },
                        count,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range
                                .map(|ix| {
                                    this.render_index_file(side, ix, &Theme::get(cx).clone(), cx)
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .min_h_0()
                    .px(dp(6.))
                    .track_scroll(scroll),
                    match side {
                        IndexSide::Unstaged => "unstaged-scrollbar",
                        IndexSide::Staged => "staged-scrollbar",
                    },
                    Scroll::Div(scroll.0.borrow().base_handle.clone()),
                    theme,
                ))
            })
            .into_any_element()
    }

    fn render_index_file(
        &self,
        side: IndexSide,
        ix: usize,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        let Some(file) = state.status.as_ref().and_then(|s| s.files(side).get(ix)) else {
            return div().into_any_element();
        };
        let path = file.file.path.clone();
        let selected = state
            .selected
            .as_ref()
            .is_some_and(|(s, p)| *s == side && *p == path);
        let drag = self.index_drag(side, Some(&path));
        let action = drag.clone();
        list_row(("index-file", ix), selected, theme)
            .mx_0()
            .w_full()
            .child(file_icon(&path, cx))
            .child(div().flex_1().min_w_0().truncate().child(path.clone()))
            .child(
                button(("index-file-action", ix), side.action())
                    .small()
                    .style(ButtonStyle::Outline)
                    .disabled_because(
                        action
                            .is_none()
                            .then_some("Wait for the staging area to load"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(drag) = action.clone() {
                            this.change_index(drag, cx);
                        }
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(state) = this.staging_mut() {
                    state.selected = Some((side, path.clone()));
                    state.fingerprint = None;
                    state.settle();
                    state.diff.scroll_to(ListOffset::default());
                }
                window.focus(&this.focus, cx);
                cx.notify();
            }))
            .when_some(drag, |el, drag| {
                el.on_drag(drag, |drag, _, _, cx| {
                    let label = drag
                        .picks
                        .first()
                        .map_or("Files".to_owned(), |pick| pick.changes.file.path.clone());
                    cx.new(|_| FileDragPreview(label.into()))
                })
            })
            .into_any_element()
    }

    fn render_index_row(&self, ix: usize, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        let Some(file) = state.file() else {
            return div().into_any_element();
        };
        let Some((side, _)) = &state.selected else {
            return div().into_any_element();
        };
        let action = side.action();
        let partial = staging::supports_partial(file) && state.ready();
        match state.rows.get(ix) {
            Some(Row::File) => diff::file_header(
                0,
                file,
                theme,
                HeaderDecor {
                    flat: true,
                    titled: true,
                    ..HeaderDecor::default()
                },
                cx,
            ),
            Some(Row::Hunk(h)) => {
                let Some(hunk) = file.hunks().get(*h) else {
                    return div().into_any_element();
                };
                let key = hunk.key;
                let part = if state.picks.iter().any(|(h, _)| *h == key) {
                    "lines"
                } else {
                    "hunk"
                };
                diff::hunk_header(
                    hunk,
                    0,
                    &[],
                    theme,
                    HeaderDecor {
                        flat: true,
                        trailing: Some(
                            h_flex()
                                .flex_none()
                                .gap(dp(8.))
                                .child(
                                    button(("index-hunk", *h), format!("{action} {part}"))
                                        .small()
                                        .style(ButtonStyle::Outline)
                                        .disabled_because((!partial).then_some(
                                            "Only whole files can be moved for this diff",
                                        ))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.index_part(key, false, cx)
                                        })),
                                )
                                .when(*side == IndexSide::Unstaged, |el| {
                                    el.child(
                                        button(
                                            ("discard-index-hunk", *h),
                                            format!("Discard {part}"),
                                        )
                                        .small()
                                        .style(ButtonStyle::Outline)
                                        .disabled_because((!partial).then_some(
                                            "This diff does not support partial changes",
                                        ))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.index_part(key, true, cx)
                                            }),
                                        ),
                                    )
                                })
                                .into_any_element(),
                        ),
                        ..HeaderDecor::default()
                    },
                )
            }
            Some(Row::Line(h, l)) => {
                let Some(hunk) = file.hunks().get(*h) else {
                    return div().into_any_element();
                };
                let Some(line) = hunk.lines.get(*l) else {
                    return div().into_any_element();
                };
                let (key, at) = (hunk.key, *l);
                let picked = state.picks.contains(&(key, at));
                let row = diff::line_row(
                    line,
                    (0, *h, *l),
                    ix + 1 == state.rows.len(),
                    theme,
                    &Layout {
                        flat: true,
                        wrap: true,
                        digits: 5,
                        ..Layout::default()
                    },
                    LineDecor::default(),
                );
                div()
                    .id(("index-line", ix))
                    .relative()
                    .child(row)
                    .when(picked, |el| {
                        el.bg(theme.selected_unfocused)
                            .child(div().absolute().inset_0().bg(theme.pop).opacity(0.14))
                    })
                    .when(partial && line.kind != LineKind::Context, |el| {
                        el.cursor(CursorStyle::Arrow)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(
                                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        window.focus(&this.focus, cx);
                                        this.pick_index_line(
                                            key,
                                            at,
                                            lines::Pick::from(event.modifiers),
                                            cx,
                                        );
                                    },
                                ),
                            )
                            .on_mouse_move(cx.listener(
                                move |this, event: &gpui::MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.pick_index_line(key, at, lines::Pick::Run, cx);
                                    }
                                },
                            ))
                    })
                    .into_any_element()
            }
            Some(Row::Note) => diff::note(file, true, theme),
            None => div().into_any_element(),
        }
    }

    fn render_index_diff(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .min_h_0()
            .bg(theme.bg_1)
            .child(
                h_flex()
                    .h(dp(42.))
                    .flex_none()
                    .px(dp(12.))
                    .gap(dp(8.))
                    .text_size(dp(12.))
                    .child(div().flex_1().min_w_0().truncate().child(if state.loading {
                        "Refreshing staging area…"
                    } else {
                        "Select changed lines, then use the hunk's actions"
                    })),
            )
            .when(state.file().is_none(), |el| {
                el.child(
                    div()
                        .p(dp(24.))
                        .text_color(theme.text_3)
                        .child("Select a staged or unstaged file"),
                )
            })
            .child(scrolled(
                list(
                    state.diff.clone(),
                    cx.processor(|this, ix, _, cx| {
                        this.render_index_row(ix, &Theme::get(cx).clone(), cx)
                    }),
                )
                .size_full()
                .min_h_0(),
                "index-diff-scrollbar",
                Scroll::List(state.diff.clone()),
                theme,
            ))
            .into_any_element()
    }

    fn render_index_composer(
        &self,
        project: &ProjectId,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        let staged = state.status.as_ref().map_or(0, |s| s.staged.len());
        let target = self.effective_target(project, cx);
        let label = target
            .as_ref()
            .map_or("Choose a branch".to_owned(), |target| {
                backend::short_ref(target).into_owned()
            });
        v_flex()
            .h(dp(state.composer_height))
            .max_h(gpui::relative(0.65))
            .flex_none()
            .bg(theme.bg_1)
            .child(
                div()
                    .id("resize-index-composer")
                    .h(dp(5.))
                    .flex_none()
                    .cursor(CursorStyle::ResizeUpDown)
                    .bg(theme.border_3)
                    .on_drag(Resize::Composer, |_, _, _, cx| cx.new(|_| EmptyView)),
            )
            .child(
                v_flex()
                    .p(dp(12.))
                    .gap(dp(8.))
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_h_0().child(self.message.clone()))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(dp(8.))
                            .text_size(dp(12.))
                            .child("Commit to")
                            .child(self.render_target_picker(label, true, theme, cx))
                            .child(div().flex_1())
                            .child(format!(
                                "{staged} staged {}",
                                if staged == 1 { "file" } else { "files" }
                            ))
                            .child(
                                button("commit-index", "Commit staged")
                                    .style(ButtonStyle::Pop)
                                    .disabled_because(
                                        (!state.ready() || staged == 0 || target.is_none())
                                            .then_some("Stage changes and choose a branch first"),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.commit_index(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_staging(
        &self,
        project: &ProjectId,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.staging() else {
            return div().into_any_element();
        };
        let width = self.store.read(cx).width(Pane::Files);
        v_flex()
            .size_full()
            .min_h_0()
            .child(self.render_checkout_header(project, &Checkout::Main, true, theme, cx))
            .when_some(state.error.clone(), |el, error| {
                el.child(div().p(dp(12.)).text_color(theme.danger).child(error))
            })
            .when(
                state.status.as_ref().is_some_and(|s| s.conflicts > 0),
                |el| {
                    el.child(
                        div()
                            .p(dp(12.))
                            .text_size(dp(12.))
                            .text_color(theme.warn)
                            .child("Resolve conflicts in Changes before staging"),
                    )
                },
            )
            .child(
                v_flex()
                    .id("index-content")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .on_drag_move(cx.listener(|this, event: &DragMoveEvent<Resize>, _, cx| {
                        if matches!(event.drag(cx), Resize::Composer) {
                            let scale = Theme::get(cx).scale;
                            let height =
                                f32::from(event.bounds.bottom() - event.event.position.y) / scale;
                            let max =
                                (f32::from(event.bounds.size.height) / scale * 0.65).max(150.);
                            if let Some(state) = this.staging_mut() {
                                state.composer_height = height.clamp(150., max);
                            }
                            cx.notify();
                        }
                    }))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .on_drag_move(resize_pane(Pane::Files, 0., self.store.clone()))
                            .child(
                                v_flex()
                                    .id("index-lists")
                                    .w(dp(width))
                                    .h_full()
                                    .min_h_0()
                                    .flex_none()
                                    .relative()
                                    .border_r_1()
                                    .border_color(theme.border_3)
                                    .on_drag_move(cx.listener(
                                        |this, event: &DragMoveEvent<Resize>, _, cx| {
                                            if matches!(event.drag(cx), Resize::Lists) {
                                                let share = f32::from(
                                                    event.event.position.y - event.bounds.top(),
                                                ) / f32::from(event.bounds.size.height);
                                                if let Some(state) = this.staging_mut() {
                                                    state.lists_share = share.clamp(0.2, 0.8);
                                                }
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .child(
                                        div()
                                            .flex_grow(state.lists_share)
                                            .flex_basis(dp(0.))
                                            .min_h_0()
                                            .child(self.render_index_list(
                                                project,
                                                IndexSide::Staged,
                                                theme,
                                                cx,
                                            )),
                                    )
                                    .child(
                                        div()
                                            .id("resize-index-lists")
                                            .h(dp(5.))
                                            .flex_none()
                                            .cursor(CursorStyle::ResizeUpDown)
                                            .bg(theme.border_3)
                                            .on_drag(Resize::Lists, |_, _, _, cx| {
                                                cx.new(|_| EmptyView)
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex_grow(1. - state.lists_share)
                                            .flex_basis(dp(0.))
                                            .min_h_0()
                                            .child(self.render_index_list(
                                                project,
                                                IndexSide::Unstaged,
                                                theme,
                                                cx,
                                            )),
                                    )
                                    .child(resize_handle(Pane::Files, self.store.clone(), theme)),
                            )
                            .child(self.render_index_diff(theme, cx)),
                    )
                    .child(self.render_index_composer(project, theme, cx)),
            )
            .into_any_element()
    }
}
