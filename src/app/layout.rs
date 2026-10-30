//! The column plan and the geometry it is built from: panel widths, the
//! shared margins and card gaps, and the resize grips' geometry.

use super::*;

/// The bar's two menus — the filter's and the sort's: a row per choice, each the
/// choice's own mark, its name, and the tick at the end that says whether it is
/// the one in force. One width for both, because they hang from adjacent cells
/// and are read as one control.
pub(super) const LIST_MENU_WIDTH: f32 = 240.0;
/// Padding on the sides of the message list's rows: the hairlines `mail_list`
/// draws down the column's sides sit just outside it.
pub(super) const LIST_MARGIN: i8 = 8;
/// The rows' padding inside the list's scroll area: the sides (`LIST_MARGIN`,
/// which keeps the mails off the hairlines the card is framed by) and the top.
/// The top is the title bar's height and it is *inside* the scroll area on
/// purpose: the first mail starts below the bar at rest, and scrolling takes
/// the padding away with the rows, which is what slides them under the floating
/// bar. It belongs to the scroll area rather than to the column, which is what
/// lets the scroll bar reach the column's edge. **There is no bottom padding**:
/// the list's card ends with its last mail (`list_card`), so a padding below
/// the rows would be a strip of surface under the last mail — and, scrolled to
/// the end, a strip of nothing under it.
pub(super) const LIST_PADDING: egui::Margin = egui::Margin {
    left: LIST_MARGIN,
    right: LIST_MARGIN,
    top: TITLEBAR_HEIGHT_I8,
    bottom: 0,
};
/// The gutter around the reading pane's card — the gap between the card and
/// the list column, and the chrome showing between the card and the panel's
/// other edges. It doubles as the splitter: egui's drag area around the panel
/// boundary covers it, and no line is drawn in it.
pub(super) const CONTENT_GUTTER: f32 = 4.0;
/// Padding between the card's edge and the mail on it — all four sides, so the
/// subject and the body do not sit against the card's corners. The integer
/// form is the one `egui::Margin` wants.
pub(super) const CONTENT_PADDING_I8: i8 = 12;
pub(super) const CONTENT_PADDING: egui::Vec2 =
    egui::vec2(CONTENT_PADDING_I8 as f32, CONTENT_PADDING_I8 as f32);
/// Corner radius of the reading pane's card.
pub(super) const CONTENT_CARD_CORNER_RADIUS: u8 = 8;
/// The room between two cards of a conversation in the reading pane, which is
/// the chrome showing between them: a conversation is one card per mail
/// (`mail_card`), each of them the card a mail on its own gets.
pub(super) const CONVERSATION_CARD_GAP: f32 = 12.0;
/// The three columns' widths. The navigation is a fixed-width panel, the
/// message list is a capped one — it starts at its default width and its
/// splitter cannot take it past `LIST_MAX_WIDTH` — and the reading pane is
/// what the two leave, so it is the column that grows with the window. The
/// list's minimum is also what the app enforces itself (`plan_columns`
/// caps the list at what leaves the pane its `CONTENT_MIN_WIDTH`) and what
/// a window narrower than it turns into a horizontal scrollbar instead
/// (`mail_list`).
pub(super) const NAV_MIN_WIDTH: f32 = 150.0;
pub(super) const NAV_DEFAULT_WIDTH: f32 = 200.0;
pub(super) const CONTENT_MIN_WIDTH: f32 = 280.0;
pub(super) const LIST_MIN_WIDTH: f32 = 240.0;
/// Where a list splitter starts, and the widest it can make the list: the
/// reading pane is the column that takes the slack, so the list stops at a
/// width its rows read well at rather than growing with the window.
pub(super) const LIST_DEFAULT_WIDTH: f32 = 460.0;
pub(super) const LIST_MAX_WIDTH: f32 = 600.0;
pub(super) fn plan_columns(window: f32, nav: f32, list: f32, nav_collapsed: bool) -> ColumnPlan {
    // What the two content columns share once the navigation has had its
    // width — all of it, when the navigation is collapsed.
    let room = window - if nav_collapsed { 0.0 } else { nav };
    // The list's share is what is left beside the reading pane's minimum;
    // the pane takes the rest. The list is the one that is capped, rather
    // than the pane, because the pane is the column that flexes.
    let list_room = room - CONTENT_MIN_WIDTH;
    let popup = list_room < LIST_MIN_WIDTH;
    ColumnPlan {
        popup,
        list_max: (!popup && list_room < list).then_some(list_room),
        list_scrolls: window < LIST_MIN_WIDTH,
        collapses_nav: window < nav + list + CONTENT_MIN_WIDTH,
    }
}
/// A resizable panel's remembered width — what its splitter left it at, as
/// egui kept it from the previous frame. A collapsed navigation keeps its
/// own, because a panel that is not shown stores nothing.
pub(super) fn panel_width(ctx: &egui::Context, panel: &str) -> Option<f32> {
    egui::PanelState::load(ctx, egui::Id::new(panel)).map(|state| state.outer_rect.width())
}
/// Place a panel at a width of the app's choosing: what a capped list is
/// held at while the cap lasts, and what it is brought back to once the cap
/// lifts (`KimoApp::ui`) — egui on its own would only ever remember what a
/// panel last rendered at. Only the size is meant; the position is a dummy.
pub(super) fn set_panel_width(ctx: &egui::Context, panel: &str, width: f32) {
    let rect = egui::Rect::from_min_size(
        egui::Pos2::ZERO,
        egui::vec2(width, ctx.viewport_rect().height()),
    );
    ctx.data_mut(|data| {
        data.insert_persisted(egui::Id::new(panel), egui::PanelState { outer_rect: rect })
    });
}
/// The chrome showing between two cards of the reading pane, a mail's and
/// the compose's alike: the room `add_space` is asked for is the room wanted
/// minus the item spacing it adds on top of what it is given.
pub(super) fn pane_card_gap(ui: &egui::Ui) -> f32 {
    (CONVERSATION_CARD_GAP - ui.spacing().item_spacing.y).max(0.0)
}
