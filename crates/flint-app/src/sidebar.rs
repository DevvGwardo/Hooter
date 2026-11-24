//! The floating sidebar: the Hooter wordmark with back/forward history,
//! search and quick actions, triage sections with rich and compact session
//! rows, and a compact utility footer.

use std::time::SystemTime;

use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::ARCHIVE_PAGE;
use crate::app::FlintApp;
use crate::app::NavBack;
use crate::app::NavForward;
use crate::app::NewSession;
use crate::app::OpenWorkspace;
use crate::app::SESSION_PAGE;
use crate::app::SessionFilter;
use crate::app::SessionGrouping;
use crate::app::SidebarMenu;
use crate::app::ToggleChanges;
use crate::app::ToggleSidebar;
use crate::app::ToggleTerminal;
use crate::project;
use crate::session::Bucket;
use crate::session::Status;
use crate::session::TriageSection;
use crate::session_groups::GroupKey;
use crate::session_groups::SessionGroups;
use crate::theme::palette;
use crate::theme::size;
use crate::ui;

/// Minimum content width for the search row and footer controls.
pub const MIN_WIDTH: f32 = 264.;

fn white(alpha: f32) -> Hsla {
    hsla(0., 0., 1., alpha)
}

struct RowFocus {
    handle: FocusHandle,
    last: Option<FocusHandle>,
}

#[derive(IntoElement)]
pub(crate) struct ScrollableRow {
    pub(crate) id: ElementId,
    pub(crate) row: Stateful<Div>,
    pub(crate) scroll: ScrollHandle,
}

impl RenderOnce for ScrollableRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id, cx, |_, cx| RowFocus {
            handle: cx.focus_handle().tab_stop(false),
            last: None,
        });
        let handle = state.read(cx).handle.clone();
        self.row
            .track_focus(&handle)
            .on_prepaint(move |bounds, window, cx| {
                let focused = window
                    .focused(cx)
                    .filter(|_| handle.contains_focused(window, cx));
                let changed = state.update(cx, |state, _| {
                    let changed = state.last != focused;
                    state.last = focused.clone();
                    changed
                });
                if changed && let Some(focused) = focused {
                    reveal_row(self.scroll, bounds, focused, window);
                }
            })
            .test_support()
    }
}

fn reveal_row(
    scroll: ScrollHandle,
    bounds: Bounds<Pixels>,
    focus: FocusHandle,
    window: &mut Window,
) {
    let viewport = scroll.bounds();
    let delta = if bounds.top() < viewport.top() {
        viewport.top() - bounds.top()
    } else if bounds.bottom() > viewport.bottom() {
        viewport.bottom() - bounds.bottom()
    } else {
        return;
    };
    let offset = scroll.offset();
    window.on_next_frame(move |window, _| {
        if focus.is_focused(window) && scroll.offset() == offset {
            scroll.set_offset(point(
                offset.x,
                (offset.y + delta).clamp(-scroll.max_offset().y, px(0.)),
            ));
            window.refresh();
        }
    });
}

pub fn render(app: &FlintApp, window: &mut Window, cx: &mut Context<FlintApp>) -> impl IntoElement {
    let p = palette();
    // A stable handle for the footer's grouping control, so choosing an order
    // from its menu can hand focus back instead of dropping it.
    let grouping_focus = window
        .use_keyed_state("sidebar-grouping-focus", cx, |_, cx| {
            cx.focus_handle().tab_stop(true)
        })
        .read(cx)
        .clone();

    // The dedicated grip moves the panel; the rest of the row moves the window.
    // The mark and the history buttons stay together on the left, the close
    // button on the right.
    let nav_back = history_button(
        "nav-back",
        IconName::ArrowLeft,
        "Back",
        "⌘[",
        !app.nav_back.is_empty(),
    )
    .on_click(cx.listener(|_, _, window, cx| {
        window.dispatch_action(Box::new(NavBack), cx);
    }))
    .test_support();
    let nav_forward = history_button(
        "nav-forward",
        IconName::ArrowRight,
        "Forward",
        "⌘]",
        !app.nav_forward.is_empty(),
    )
    .on_click(cx.listener(|_, _, window, cx| {
        window.dispatch_action(Box::new(NavForward), cx);
    }))
    .test_support();
    let top = app.drag_region(
        div()
            .id("sidebar-top")
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .child(crate::docking::handle(crate::docking::Panel::Sidebar, cx))
            .child(
                ui::label("Hooter", size::SM, p.text)
                    .id("sidebar-heading")
                    .font_weight(FontWeight::MEDIUM)
                    .test_support(),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(nav_back)
                    .child(nav_forward),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("hide-sidebar")
                    .role(gpui_kit::Role::Button)
                    .aria_label("Close sessions panel")
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(p.accent))
                    .size(px(32.))
                    .rounded(px(8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(white(0.06)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.session_drawer {
                            this.close_session_drawer(window, cx);
                        } else {
                            window.dispatch_action(Box::new(ToggleSidebar), cx);
                        }
                    }))
                    .child(ui::icon(IconName::PanelLeft, 16., p.text_subtle))
                    .test_support(),
            ),
        cx,
    );

    // Search shares one row with the two occasional icon actions; the big
    // accent bar and the terminal moved out (the footer carries the terminal).
    let new_agent = icon_button("new-agent", IconName::SquarePen, "New agent", "⌘N")
        .on_click(cx.listener(|_, _, window, cx| {
            window.dispatch_action(Box::new(NewSession), cx);
        }))
        .test_support();
    let open_folder = icon_button("open-folder", IconName::FolderPlus, "Open folder", "⌘O")
        .on_click(cx.listener(|_, _, window, cx| {
            window.dispatch_action(Box::new(OpenWorkspace), cx);
        }))
        .test_support();
    let search = div()
        .id("session-search-row")
        .flex_shrink_0()
        .mx(px(10.))
        .mb(px(8.))
        .h(px(34.))
        .flex()
        .items_center()
        .gap(px(6.))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(&app.search)
                    .id("session-search")
                    .prefix(ui::icon(IconName::Search, 14., p.text_subtle))
                    .cleanable(true)
                    .small(),
            ),
        )
        .child(open_folder)
        .child(new_agent)
        .test_support();

    // Headings per the chosen grouping. A heading counts "n of N" when
    // search, the filter or paging hides some of its sessions.
    let SessionGroups { groups, more } = app.session_groups(cx);
    let mut list: Vec<AnyElement> = Vec::new();
    for (g, group) in groups.into_iter().enumerate() {
        let triage = matches!(group.key, GroupKey::Triage(_));
        let icon = match group.key {
            GroupKey::Triage(TriageSection::Attention) => {
                ui::icon(IconName::ShieldAlert, 13., p.warning)
            }
            GroupKey::Triage(TriageSection::Working) => ui::spinner(app.now(), 13., p.accent),
            GroupKey::Triage(TriageSection::Settled) => {
                ui::icon(IconName::Check, 13., p.text_subtle)
            }
            GroupKey::Project(..) => ui::icon(IconName::Folder, 13., p.text_subtle),
            GroupKey::Status(Bucket::NeedsYou) => ui::icon(IconName::ShieldAlert, 13., p.warning),
            GroupKey::Status(Bucket::Working) => ui::icon(IconName::Loader, 13., p.accent),
            GroupKey::Status(Bucket::Ready) => ui::icon(IconName::Check, 13., p.text_subtle),
            GroupKey::Status(Bucket::Inactive) => ui::icon(IconName::Circle, 13., p.text_subtle),
            GroupKey::Agent(_) => ui::icon(IconName::Bot, 13., p.text_subtle),
        };
        let shown = group.rows.len();
        let total = group.total;
        let tooltip = group.tooltip.clone();
        // Project and agent headings say how many of theirs are working or
        // waiting, so a busy group stands out even when scrolled past. Triage
        // headings already are the work, so a plain count is enough.
        let (working, waiting) = if triage || matches!(group.key, GroupKey::Status(_)) {
            (0, 0)
        } else {
            group.rows.iter().fold((0, 0), |(w, n), &ix| {
                match app.sessions[ix].status().bucket() {
                    Bucket::Working => (w + 1, n),
                    Bucket::NeedsYou => (w, n + 1),
                    Bucket::Ready | Bucket::Inactive => (w, n),
                }
            })
        };
        let rows: Vec<_> = group
            .rows
            .into_iter()
            .map(|ix| session_row(app, ix, cx))
            .collect();
        // A collapsed heading keeps its counts and hides its rows.
        let collapsed = app.collapsed_groups.contains(&group.key);
        let key = group.key.clone();
        list.push(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .id(("group-header", g))
                        .role(gpui_kit::Role::Button)
                        .aria_label(format!(
                            "{}, {}",
                            group.label,
                            if collapsed { "collapsed" } else { "expanded" }
                        ))
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(p.accent))
                        .mx(px(8.))
                        .px(px(12.))
                        .h(px(24.))
                        .mt(px(if g == 0 { 8. } else { 4. }))
                        .rounded(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .hover(|style| style.bg(white(0.04)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_group_collapsed(key.clone(), cx)
                        }))
                        .child(
                            div()
                                .id(("group-toggle", g))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .child(ui::icon(
                                    if collapsed {
                                        IconName::ChevronRight
                                    } else {
                                        IconName::ChevronDown
                                    },
                                    11.,
                                    p.text_subtle,
                                ))
                                .test_support(),
                        )
                        .child(icon)
                        .child(
                            div()
                                .id(("workspace-label", g))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(size::XS))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text_subtle)
                                .when_some(tooltip, |label, tip| {
                                    label.tooltip(move |window, cx| {
                                        Tooltip::new(tip.clone()).build(window, cx)
                                    })
                                })
                                .child(group.label)
                                .test_support(),
                        )
                        .when(waiting > 0, |header| {
                            header.child(
                                div()
                                    .id(("group-waiting", g))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(3.))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(format!("{waiting} need you"))
                                            .build(window, cx)
                                    })
                                    .child(ui::icon(IconName::CircleAlert, 11., p.warning))
                                    .child(ui::label(waiting.to_string(), size::XS, p.warning))
                                    .test_support(),
                            )
                        })
                        .when(working > 0, |header| {
                            header.child(
                                div()
                                    .id(("group-working", g))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(3.))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(format!("{working} working")).build(window, cx)
                                    })
                                    .child(ui::spinner(app.now(), 11., p.accent))
                                    .child(ui::label(working.to_string(), size::XS, p.accent))
                                    .test_support(),
                            )
                        })
                        // The count keeps its right-aligned slot, so a changing
                        // number can never move the heading or its rows.
                        .child(
                            div()
                                .id(("group-count", g))
                                .flex_shrink_0()
                                .child(ui::label(
                                    if shown < total {
                                        format!("{shown} of {total}")
                                    } else {
                                        shown.to_string()
                                    },
                                    size::XS,
                                    p.text_subtle,
                                ))
                                .test_support(),
                        )
                        .test_support(),
                )
                .children(if collapsed { Vec::new() } else { rows })
                .into_any_element(),
        );
    }
    if more > 0 {
        list.push(
            show_more(
                "show-more-sessions",
                more.min(SESSION_PAGE),
                more,
                cx.listener(|this, _, _, cx| {
                    this.session_limit += SESSION_PAGE;
                    cx.notify();
                }),
            )
            .into_any_element(),
        );
    }
    let empty = list.is_empty();
    let searching = !app.search.read(cx).value().trim().is_empty();
    let empty_label = if searching {
        "No matching sessions"
    } else {
        match app.filter {
            SessionFilter::All => "No sessions",
            SessionFilter::Only(Bucket::NeedsYou) => "Nothing needs you",
            SessionFilter::Only(Bucket::Working) => "No sessions working",
            SessionFilter::Only(Bucket::Ready) => "No sessions ready",
            SessionFilter::Only(Bucket::Inactive) => "No inactive sessions",
        }
    };

    let grouping = app.grouping;
    let group_toggle = div()
        .id("session-grouping")
        .role(gpui_kit::Role::Button)
        .aria_label(format!("Group by {}", grouping.label()))
        .track_focus(&grouping_focus)
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(p.accent))
        .tooltip(move |window, cx| {
            Tooltip::new(format!(
                "Grouped by {}  {}\nChoose an order",
                grouping.label(),
                crate::shortcuts::label("⇧⌘G")
            ))
            .build(window, cx)
        })
        .size(px(28.))
        .flex_shrink_0()
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(app.sidebar_menu == Some(SidebarMenu::Grouping), |button| {
            button.bg(p.raised)
        })
        .hover(|style| style.bg(white(0.06)))
        .on_click(cx.listener(|this, _, _, cx| {
            this.sidebar_menu =
                (this.sidebar_menu != Some(SidebarMenu::Grouping)).then_some(SidebarMenu::Grouping);
            cx.notify();
        }))
        .child(ui::icon(IconName::ArrowUpDown, 16., p.text_subtle))
        .test_support();
    // The grouping picker opens above the footer row: the footer is the last
    // child of a fixed-height panel, so a panel below it would be clipped.
    let grouping_menu = (app.sidebar_menu == Some(SidebarMenu::Grouping)).then(|| {
        div()
            .id("grouping-menu")
            .mx(px(8.))
            .mb(px(4.))
            .p(px(4.))
            .rounded(px(10.))
            .border_1()
            .border_color(p.border_strong)
            .bg(p.surface)
            .shadow_lg()
            .children(
                SessionGrouping::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(ix, option)| {
                        let selected = option == app.grouping;
                        let grouping_focus = grouping_focus.clone();
                        div()
                            .id(("grouping-option", ix))
                            .role(gpui_kit::Role::Button)
                            .aria_label(option.label())
                            .tab_index(0)
                            .focus_visible(|style| style.border_2().border_color(p.accent))
                            .h(px(30.))
                            .px(px(8.))
                            .rounded(px(7.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .cursor_pointer()
                            .hover(|style| style.bg(white(0.06)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.sidebar_menu = None;
                                this.set_grouping(option, cx);
                                grouping_focus.focus(window, cx);
                            }))
                            .child(
                                div()
                                    .w(px(13.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .child(if selected {
                                        ui::icon(IconName::Check, 13., p.accent).into_any_element()
                                    } else {
                                        div().into_any_element()
                                    }),
                            )
                            .child(div().flex_1().min_w_0().truncate().child(ui::label(
                                option.label(),
                                size::SM,
                                if selected { p.text } else { p.text_muted },
                            )))
                            .test_support()
                    }),
            )
            .child(div().px(px(8.)).pt(px(4.)).child(ui::label(
                format!("{} cycles", crate::shortcuts::label("⇧⌘G")),
                size::XS,
                p.text_subtle,
            )))
            .test_support()
    });

    // The footer is one compact icon bar: utilities on the left, list order
    // and the archive drawer on the right.
    let settings = icon_button("settings", IconName::Settings, "Settings", "⌘,")
        .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx)))
        .test_support();
    let changes = icon_button("sidebar-changes", IconName::FileDiff, "Changes", "⌘J")
        .when(app.changes_open, |button| button.bg(white(0.075)))
        .on_click(cx.listener(|_, _, window, cx| {
            window.dispatch_action(Box::new(ToggleChanges), cx);
        }))
        .test_support();
    let terminal = icon_button("terminal", IconName::SquareTerminal, "Terminal", "⌃`")
        .when(app.terminal.open, |button| button.bg(white(0.075)))
        .on_click(cx.listener(|_, _, window, cx| {
            window.dispatch_action(Box::new(ToggleTerminal), cx);
        }))
        .test_support();
    let archived_toggle = div()
        .id("archived-sessions")
        .role(gpui_kit::Role::Button)
        .aria_label(format!("Archived, {} sessions", app.archives.len()))
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(p.accent))
        .size(px(28.))
        .flex_shrink_0()
        .relative()
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(app.archived_open, |button| button.bg(white(0.075)))
        .hover(|style| style.bg(white(0.06)))
        .tooltip(|window, cx| Tooltip::new("Archived sessions").build(window, cx))
        .on_click(cx.listener(|this, _, _, cx| this.toggle_archived(cx)))
        .child(ui::icon(IconName::Archive, 16., p.text_muted))
        .child(div().absolute().top(px(2.)).right(px(2.)).child(ui::label(
            if app.archives.len() > 99 {
                "99+".to_string()
            } else {
                app.archives.len().to_string()
            },
            9.,
            p.text_subtle,
        )))
        .test_support();
    let footer = div()
        .id("sidebar-footer")
        .flex_shrink_0()
        .h(px(38.))
        .border_t_1()
        .border_color(white(0.05))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(2.))
        .child(settings)
        .child(changes)
        .child(terminal)
        .child(div().flex_1())
        .child(group_toggle)
        .when(!app.archives.is_empty(), |bar| bar.child(archived_toggle))
        .test_support();
    let bottom = div()
        .flex_shrink_0()
        .when_some(grouping_menu, |bottom, menu| bottom.child(menu))
        .child(footer);

    // The archive drawer keeps its own scroll area, opened from the footer.
    let archived = (!app.archives.is_empty() && app.archived_open).then(|| {
        div()
            .relative()
            .flex_shrink_0()
            .border_t_1()
            .border_color(white(0.05))
            .child(
                div()
                    .id("archive-list")
                    // The compact drawer loses the titlebar's height;
                    // keep that space available to current sessions.
                    .max_h(px(if app.session_drawer && !app.sidebar_visible {
                        76.
                    } else {
                        112.
                    }))
                    .overflow_y_scroll()
                    .track_scroll(&app.archive_scroll)
                    .pr(px(12.))
                    .children(app.archives.iter().enumerate().take(app.archive_limit).map(
                        |(ix, (_, meta))| {
                            let title = meta.title.as_deref().unwrap_or("session").to_string();
                            let tip = format!("Restore {title}\n{}", meta.workspace.display());
                            let row = div()
                                .id(("restore-archive", ix))
                                .role(gpui_kit::Role::Button)
                                .aria_label(format!("Restore {title}"))
                                .tab_index(0)
                                .focus_visible(|style| style.border_2().border_color(p.accent))
                                .mx(px(8.))
                                .px(px(12.))
                                .h(px(34.))
                                .rounded(px(7.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .hover(|style| style.bg(white(0.05)))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.restore_archive(ix, window, cx)
                                }))
                                .child(ui::icon(IconName::ArchiveRestore, 14., p.text_muted))
                                .child(div().flex_1().min_w_0().truncate().child(ui::label(
                                    title,
                                    size::SM,
                                    p.text,
                                )))
                                .test_support();
                            ScrollableRow {
                                id: ("archive-scroll-row", ix).into(),
                                row: div().id(("archive-container", ix)).child(row),
                                scroll: app.archive_scroll.clone(),
                            }
                        },
                    ))
                    .when(app.archives.len() > app.archive_limit, |list| {
                        let hidden = app.archives.len() - app.archive_limit;
                        list.child(show_more(
                            "show-more-archives",
                            hidden.min(ARCHIVE_PAGE),
                            hidden,
                            cx.listener(|this, _, _, cx| {
                                this.archive_limit += ARCHIVE_PAGE;
                                cx.notify();
                            }),
                        ))
                    })
                    .test_support(),
            )
            .child(Scrollbar::vertical(&app.archive_scroll).mode(ScrollbarMode::Always))
    });

    div()
        .id("sidebar")
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .rounded(px(18.))
        .bg(if app.session_drawer && !app.sidebar_visible {
            p.chrome
        } else {
            p.sidebar_fill
        })
        .border_1()
        .border_color(p.border)
        .overflow_hidden()
        .child(top)
        .child(search)
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .id("session-list")
                        .h_full()
                        .overflow_y_scroll()
                        .track_scroll(&app.sidebar_scroll)
                        .pr(px(12.))
                        .pb(px(8.))
                        .children(list)
                        .when(empty, |list| {
                            list.child(
                                div()
                                    .id("session-list-empty")
                                    .p(px(20.))
                                    .flex()
                                    .flex_col()
                                    .gap(px(12.))
                                    .child(ui::label(empty_label, size::SM, p.text_muted))
                                    .child(
                                        gpui_kit::component::button::Button::new(
                                            "reset-session-filter",
                                        )
                                        .label("Show all sessions")
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.filter = SessionFilter::All;
                                                this.session_menu = None;
                                                this.sidebar_scroll.set_offset(Point::default());
                                                this.search.update(cx, |state, cx| {
                                                    state.set_value("", window, cx);
                                                    state.focus(window, cx);
                                                });
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .test_support(),
                            )
                        })
                        .test_support(),
                )
                .child(Scrollbar::vertical(&app.sidebar_scroll).mode(ScrollbarMode::Always)),
        )
        .children(archived)
        .child(bottom)
        .test_support()
}

/// The row that reveals the next page of a long list.
fn show_more(
    id: &'static str,
    next: usize,
    hidden: usize,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let p = palette();
    div()
        .id(id)
        .role(gpui_kit::Role::Button)
        .aria_label(format!("Show {next} more"))
        .tab_index(0)
        .focus_visible(|style| style.border_2().border_color(p.accent))
        .mx(px(8.))
        .mt(px(8.))
        .px(px(12.))
        .h(px(32.))
        .rounded(px(8.))
        .flex()
        .items_center()
        .justify_between()
        .cursor_pointer()
        .hover(|style| style.bg(white(0.05)))
        .on_click(on_click)
        .child(ui::label(
            format!("Show {next} more"),
            size::SM,
            p.text_muted,
        ))
        .child(ui::label(
            format!("{hidden} hidden"),
            size::XS,
            p.text_subtle,
        ))
        .test_support()
}

/// A 28 px icon-only button for the sidebar's secondary actions.
fn icon_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    hint: &'static str,
) -> Stateful<Div> {
    icon_button_tinted(id, icon, label, hint, palette().text_muted)
}

fn icon_button_tinted(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    hint: &'static str,
    color: Hsla,
) -> Stateful<Div> {
    let p = palette();
    div()
        .id(id)
        .role(gpui_kit::Role::Button)
        .aria_label(label)
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(p.accent))
        .size(px(28.))
        .flex_shrink_0()
        .rounded(px(7.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(white(0.06)))
        .tooltip(move |window, cx| Tooltip::new(format!("{label}  {hint}")).build(window, cx))
        .child(ui::icon(icon, 16., color))
}

/// A history button: as large as the other icon actions, but visibly dimmed
/// once there is nowhere to go. It stays clickable; the handler is a no-op.
fn history_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    hint: &'static str,
    enabled: bool,
) -> Stateful<Div> {
    let p = palette();
    let color = if enabled {
        p.text_muted
    } else {
        p.text_subtle.opacity(0.35)
    };
    icon_button_tinted(id, icon, label, hint, color)
}

fn session_row(app: &FlintApp, ix: usize, cx: &mut Context<FlintApp>) -> AnyElement {
    let p = palette();
    let session = &app.sessions[ix];
    let active = ix == app.active && session.selected_subagent.is_none();
    let now = app.now();
    let status = session.status();
    let compact = session.triage_section() == TriageSection::Settled;
    let general = session.general;
    let project_label = project::identity(&session.workspace).label();
    let branch = project::branch(&session.workspace);
    let agent_label = session.agent.label().to_string();
    let tokens = session.total_tokens();
    let full_title = session.title();
    // A running session shows how long its turn has taken; others, when they
    // were last active.
    let elapsed = session.running_for(now);
    let (when, when_color) = match elapsed {
        Some(elapsed) => (crate::session::clock(elapsed), p.accent),
        None => (
            ui::ago(
                SystemTime::now()
                    .duration_since(session.touched)
                    .unwrap_or_default(),
            ),
            p.text_subtle,
        ),
    };
    let (status_line, _) = session.status_line(now);
    let emphasized = active
        || matches!(
            status,
            Status::Unread
                | Status::NeedsApproval
                | Status::NeedsReview
                | Status::Blocked
                | Status::Failed { seen: false }
        );
    let renaming = app
        .renaming
        .as_ref()
        .filter(|(row, _)| *row == ix)
        .map(|(_, input)| input.clone());
    let is_renaming = renaming.is_some();
    // The branch and token diagnostics live on the third line and the tooltip;
    // the title stays one clean line.
    let title_tooltip = {
        let mut tip = full_title.clone();
        if !status_line.is_empty() {
            tip.push('\n');
            tip.push_str(&status_line);
        }
        if let Some(branch) = &branch {
            tip.push('\n');
            tip.push_str(branch);
        }
        if tokens > 0 {
            tip.push_str(&format!("\n{} tokens", ui::tokens(tokens)));
        }
        tip
    };
    let title_size = if compact { size::XS } else { size::BASE };
    let title: AnyElement = match renaming {
        Some(input) => {
            let focus = input.focus_handle(cx);
            let scroll = app.sidebar_scroll.clone();
            let reveal = app.rename_needs_scroll.replace(false);
            div()
                .id(("rename-container", ix))
                .on_prepaint(move |bounds, window, _| {
                    if reveal {
                        reveal_row(scroll, bounds, focus, window);
                    }
                })
                .flex_1()
                .min_w_0()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(&input).id(("rename-input", ix)).small())
                .into_any_element()
        }
        None => div()
            .id(("sidebar-title", ix))
            .flex_1()
            .min_w_0()
            .truncate()
            .text_size(px(title_size))
            .text_color(if emphasized { p.text } else { p.text_muted })
            .when(!compact || emphasized, |t| {
                t.font_weight(FontWeight::MEDIUM)
            })
            .tooltip(move |window, cx| Tooltip::new(title_tooltip.clone()).build(window, cx))
            .child(full_title.clone())
            .test_support()
            .into_any_element(),
    };
    let row_group = format!("session-row-{ix}");
    let settle_hint = if session.settled {
        "Unsettle"
    } else {
        "Settle"
    };
    // Bounds are pinned by the fixed-size slots; only the glyphs fade in, so
    // the title never shifts when the actions appear.
    let reveal_actions = active || app.session_menu == Some(ix);
    let actions = div()
        .id(("session-actions", ix))
        .role(gpui_kit::Role::Button)
        .aria_label(format!("Actions for {}", session.title()))
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(p.accent))
        .size(px(24.))
        .flex_shrink_0()
        .rounded(px(5.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(white(0.08)))
        .tooltip(|window, cx| Tooltip::new("Session actions").build(window, cx))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.session_menu = if this.session_menu == Some(ix) {
                None
            } else {
                Some(ix)
            };
            cx.stop_propagation();
            cx.notify();
        }))
        .child(
            div()
                .opacity(if reveal_actions { 1. } else { 0. })
                .group_hover(row_group.clone(), |style| style.opacity(1.))
                .child(ui::icon(IconName::Ellipsis, 14., p.text_muted)),
        )
        .test_support();

    let row = if compact {
        let row = div()
            .id(("session", ix))
            .group(row_group.clone())
            .role(gpui_kit::Role::Button)
            .aria_label(format!("Session: {}", session.title()))
            .tab_index(if is_renaming { -1 } else { 0 })
            .focus_visible(|style| style.border_2().border_color(p.accent))
            .mx(px(8.))
            .px(px(10.))
            .h(px(if is_renaming { 34. } else { 30. }))
            .rounded(px(7.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .when(active, |row| {
                row.bg(p.raised)
                    .border_1()
                    .border_color(p.accent.opacity(0.16))
            })
            .when(!active, |row| row.hover(|style| style.bg(white(0.04))))
            .on_click(cx.listener(move |this, _, window, cx| {
                if !is_renaming {
                    this.select_session(ix, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.session_menu = Some(ix);
                    cx.notify();
                }),
            )
            .child(ui::icon(
                if general {
                    IconName::Sparkles
                } else {
                    IconName::Folder
                },
                14.,
                p.text_subtle,
            ))
            .child(title)
            .child(
                div()
                    .id(("sidebar-when", ix))
                    .flex_shrink_0()
                    .child(ui::label(when.clone(), size::XS, when_color))
                    .test_support(),
            )
            .child(actions)
            .test_support();
        let row = if is_renaming {
            row
        } else {
            crate::session_workspace::drag_row(row, session.uid, session.title(), cx)
        };
        row.into_any_element()
    } else {
        // The rich card: project and live status, title, then branch/agent
        // context with the row actions. Three quiet lines, one warm surface.
        let status_right: AnyElement = match status {
            Status::NeedsApproval => {
                ui::pill("? Input", p.warning, p.warning.opacity(0.14)).into_any_element()
            }
            Status::NeedsReview | Status::Blocked => {
                ui::pill("Review", p.warning, p.warning.opacity(0.14)).into_any_element()
            }
            Status::Failed { .. } => {
                ui::pill("Failed", p.danger, p.danger.opacity(0.14)).into_any_element()
            }
            Status::Running | Status::Starting => div()
                .flex()
                .items_center()
                .gap(px(4.))
                .child(ui::spinner(now, 11., p.accent))
                .child(ui::label(
                    if elapsed.is_some() {
                        format!("Working {when}")
                    } else {
                        "Working".to_string()
                    },
                    size::XS,
                    p.accent,
                ))
                .into_any_element(),
            _ => ui::label(when.clone(), size::XS, when_color).into_any_element(),
        };
        let context: AnyElement = match &branch {
            Some(branch) => {
                let tip = branch.clone();
                div()
                    .id(("sidebar-branch", ix))
                    .flex_shrink_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .text_size(px(size::XS))
                    .text_color(p.text_subtle)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(ui::icon(IconName::GitBranch, 11., p.text_subtle))
                    .child(div().min_w_0().truncate().child(branch.clone()))
                    .test_support()
                    .into_any_element()
            }
            None => div()
                .id(("sidebar-subtitle", ix))
                .flex_shrink_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(3.))
                .truncate()
                .text_size(px(size::XS))
                .text_color(p.text_subtle)
                .child(ui::icon(IconName::Bot, 11., p.text_subtle))
                .child(div().min_w_0().truncate().child(agent_label.clone()))
                .test_support()
                .into_any_element(),
        };
        let quick_settle = div()
            .id(("quick-settle", ix))
            .role(gpui_kit::Role::Button)
            .aria_label(settle_hint)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(p.accent))
            .size(px(24.))
            .flex_shrink_0()
            .rounded(px(5.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.bg(white(0.08)))
            .tooltip(move |window, cx| Tooltip::new(settle_hint).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_settled(ix, cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .opacity(if reveal_actions { 1. } else { 0. })
                    .group_hover(row_group.clone(), |style| style.opacity(1.))
                    .child(ui::icon(
                        if session.settled {
                            IconName::Check
                        } else {
                            IconName::Archive
                        },
                        14.,
                        p.text_muted,
                    )),
            )
            .test_support();
        let row = div()
            .id(("session", ix))
            .group(row_group)
            .role(gpui_kit::Role::Button)
            .aria_label(format!("Session: {}", session.title()))
            .tab_index(if is_renaming { -1 } else { 0 })
            .focus_visible(|style| style.border_2().border_color(p.accent))
            .mx(px(8.))
            .px(px(10.))
            .py(px(4.))
            .rounded(px(8.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .cursor_pointer()
            .when(active, |row| {
                row.bg(p.accent_soft)
                    .border_1()
                    .border_color(p.accent.opacity(0.16))
            })
            .when(!active, |row| row.hover(|style| style.bg(white(0.04))))
            .on_click(cx.listener(move |this, _, window, cx| {
                if !is_renaming {
                    this.select_session(ix, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.session_menu = Some(ix);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(ui::icon(
                        if general {
                            IconName::Sparkles
                        } else {
                            IconName::Folder
                        },
                        12.,
                        p.text_subtle,
                    ))
                    .child(
                        div()
                            .id(("sidebar-project", ix))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(size::XS))
                            .text_color(p.text_muted)
                            .child(project_label)
                            .test_support(),
                    )
                    .child(
                        div()
                            .id(("sidebar-when", ix))
                            .flex_shrink_0()
                            .child(status_right)
                            .test_support(),
                    ),
            )
            .child(title)
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(div().flex_1().min_w_0().flex().child(context))
                    .child(actions)
                    .child(quick_settle),
            )
            .test_support();
        if is_renaming {
            row.into_any_element()
        } else {
            crate::session_workspace::drag_row(row, session.uid, session.title(), cx)
                .into_any_element()
        }
    };

    let item = |id: &'static str, icon: IconName, label: &'static str, color: Hsla| {
        div()
            .id((id, ix))
            .role(gpui_kit::Role::Button)
            .aria_label(label)
            .tab_index(0)
            .focus_visible(|style| style.border_2().border_color(p.accent))
            .h(px(34.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(10.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(white(0.06)))
            .child(ui::icon(icon, 14., color))
            .child(div().flex_1().min_w_0().truncate().child(ui::label(
                label,
                size::BASE - 1.,
                color,
            )))
    };
    let container = div()
        .id(("session-container", session.uid))
        .flex()
        .flex_col()
        .child(row)
        .when(app.session_menu == Some(ix), |container| {
            container.child(
                div()
                    .mx(px(14.))
                    .my(px(4.))
                    .p(px(4.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(p.border_strong)
                    .bg(p.surface)
                    .shadow_lg()
                    .child(
                        item(
                            "settle-session",
                            IconName::Check,
                            if session.settled {
                                "Unsettle"
                            } else {
                                "Settle"
                            },
                            p.text,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_settled(ix, cx)))
                        .test_support(),
                    )
                    .child(
                        item("rename-session", IconName::Pencil, "Rename", p.text)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_rename(ix, window, cx)
                            }))
                            .test_support(),
                    )
                    .child(
                        item(
                            "delete-session",
                            IconName::Archive,
                            if session.stopping {
                                "Archive after engine saves"
                            } else {
                                "Archive"
                            },
                            p.danger,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.delete_session(ix, window, cx)
                        }))
                        .test_support(),
                    ),
            )
        });
    div()
        .flex()
        .flex_col()
        .child(ScrollableRow {
            id: ("session-scroll-row", session.uid).into(),
            row: container,
            scroll: app.sidebar_scroll.clone(),
        })
        .when(!session.view.subagents.is_empty(), |container| {
            container.child(crate::subagent_ui::sidebar_children(app, session, cx))
        })
        .into_any_element()
}
