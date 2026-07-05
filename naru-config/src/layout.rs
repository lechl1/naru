use std::str::FromStr;

use knuffel::errors::DecodeError;
use naru_ipc::{ColumnDisplay, OpenInFixedSide, SizeChange};

use crate::appearance::{
    Border, FocusRing, InsertHint, Shadow, TabIndicator, DEFAULT_BACKGROUND_COLOR,
};
use crate::utils::{expect_only_children, Flag, MergeWith};
use crate::{BorderRule, Color, FloatOrInt, InsertHintPart, ShadowRule, TabIndicatorPart};

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub focus_ring: FocusRing,
    pub border: Border,
    pub shadow: Shadow,
    pub tab_indicator: TabIndicator,
    pub insert_hint: InsertHint,
    pub preset_column_widths: Vec<PresetSize>,
    pub default_column_width: Option<PresetSize>,
    pub preset_window_heights: Vec<PresetSize>,
    pub center_focused_column: CenterFocusedColumn,
    pub always_center_single_column: bool,
    /// If true, the scrolling carousel is disabled: every workspace's columns
    /// must fit in the area between the fixed-side panels. New windows pick
    /// the widest entry in [`Self::preset_column_widths`] that still leaves
    /// room for all columns (so all columns end up at a uniform width);
    /// windows that would not fit even at the narrowest preset fall back to
    /// stacking (new row on landscape, new column on portrait). The
    /// per-pixel carousel edge-fade against the
    /// fixed-side panels is also skipped — with no scrolling there's no
    /// content to dissolve behind the panels.
    pub disable_carousel: bool,
    /// After any layout change, recompute the workspace view offset so that
    /// either: (a) all columns are horizontally centered as a group when they
    /// fit inside the working area, or (b) the view is clamped so neither
    /// edge has wasted empty space when they overflow.
    pub auto_fit_or_center: bool,
    pub empty_workspace_above_first: bool,
    pub default_column_display: ColumnDisplay,
    pub gaps: f64,
    pub struts: Struts,
    pub background_color: Color,
    /// XDG app IDs treated as terminals. Used for the ultrawide-only narrower default width
    /// (`ultrawide_terminal_column_width`) when a window in this list opens on an output with
    /// aspect ratio ≥ 21:9. Default is empty — populate via
    /// `layout { terminal-app-ids "foot" "alacritty" ... }`.
    pub terminal_app_ids: Vec<String>,
    /// Default column width for non-terminal windows on ultrawide outputs (≥ 21:9), used only
    /// when the global `default-column-width` is unset. Default is 1/3 of the view width.
    pub ultrawide_default_column_width: PresetSize,
    /// Default column width for terminal windows (matched against `terminal_app_ids`) on
    /// ultrawide outputs (≥ 21:9), used only when the global `default-column-width` is unset.
    /// Default is 1/5 of the view width.
    pub ultrawide_terminal_column_width: PresetSize,
    /// XDG app IDs treated as media players. A window in this list opens at
    /// `media_player_ultrawide_column_width` (default 1/5) on ultrawide outputs (≥ 21:9), or
    /// `media_player_column_width` (default 1/3) otherwise — overriding the global
    /// `default-column-width` — and, unless a window-rule already sets `open-in-fixed-side`, is
    /// routed into the `media_player_open_in_fixed_side` panel (default left). Default is empty;
    /// populate via `layout { media-player-app-ids "mpv" "vlc" ... }`.
    pub media_player_app_ids: Vec<String>,
    /// Default column width for media-player windows on non-ultrawide outputs. Applied over the
    /// global `default-column-width` unless a window-rule sets one explicitly. Default is 1/3.
    pub media_player_column_width: PresetSize,
    /// Default column width for media-player windows on ultrawide outputs (≥ 21:9). Applied over
    /// the global `default-column-width` unless a window-rule sets one explicitly. Default is 1/5.
    pub media_player_ultrawide_column_width: PresetSize,
    /// Fixed-side panel that media-player windows prefer to open in, unless a window-rule sets
    /// `open-in-fixed-side` explicitly. `None` disables the routing (media players then open in
    /// the carousel like any other window). Default is `Some(Left)`.
    pub media_player_open_in_fixed_side: Option<OpenInFixedSide>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            focus_ring: FocusRing::default(),
            border: Border::default(),
            shadow: Shadow::default(),
            tab_indicator: TabIndicator::default(),
            insert_hint: InsertHint::default(),
            preset_column_widths: vec![
                PresetSize::Proportion(1. / 3.),
                PresetSize::Proportion(0.5),
                PresetSize::Proportion(2. / 3.),
            ],
            default_column_width: Some(PresetSize::Proportion(0.5)),
            center_focused_column: CenterFocusedColumn::Never,
            always_center_single_column: false,
            disable_carousel: false,
            auto_fit_or_center: false,
            empty_workspace_above_first: false,
            default_column_display: ColumnDisplay::Normal,
            gaps: 16.,
            struts: Struts::default(),
            preset_window_heights: vec![
                PresetSize::Proportion(1. / 3.),
                PresetSize::Proportion(0.5),
                PresetSize::Proportion(2. / 3.),
            ],
            background_color: DEFAULT_BACKGROUND_COLOR,
            terminal_app_ids: Vec::new(),
            ultrawide_default_column_width: PresetSize::Proportion(1. / 3.),
            ultrawide_terminal_column_width: PresetSize::Proportion(1. / 5.),
            media_player_app_ids: Vec::new(),
            media_player_column_width: PresetSize::Proportion(1. / 3.),
            media_player_ultrawide_column_width: PresetSize::Proportion(1. / 5.),
            media_player_open_in_fixed_side: Some(OpenInFixedSide::Left),
        }
    }
}

impl MergeWith<LayoutPart> for Layout {
    fn merge_with(&mut self, part: &LayoutPart) {
        merge!(
            (self, part),
            focus_ring,
            border,
            shadow,
            tab_indicator,
            insert_hint,
            always_center_single_column,
            disable_carousel,
            auto_fit_or_center,
            empty_workspace_above_first,
            gaps,
        );

        merge_clone!(
            (self, part),
            preset_column_widths,
            preset_window_heights,
            center_focused_column,
            default_column_display,
            struts,
            background_color,
        );

        if let Some(x) = part.default_column_width {
            self.default_column_width = x.0;
        }

        if let Some(x) = &part.terminal_app_ids {
            self.terminal_app_ids = x.0.clone();
        }

        if let Some(x) = part.ultrawide_default_column_width {
            if let Some(v) = x.0 {
                self.ultrawide_default_column_width = v;
            }
        }

        if let Some(x) = part.ultrawide_terminal_column_width {
            if let Some(v) = x.0 {
                self.ultrawide_terminal_column_width = v;
            }
        }

        if let Some(x) = &part.media_player_app_ids {
            self.media_player_app_ids = x.0.clone();
        }

        if let Some(x) = part.media_player_column_width {
            if let Some(v) = x.0 {
                self.media_player_column_width = v;
            }
        }

        if let Some(x) = part.media_player_ultrawide_column_width {
            if let Some(v) = x.0 {
                self.media_player_ultrawide_column_width = v;
            }
        }

        if let Some(x) = &part.media_player_open_in_fixed_side {
            self.media_player_open_in_fixed_side = x.to_option();
        }

        if self.preset_column_widths.is_empty() {
            self.preset_column_widths = Layout::default().preset_column_widths;
        }

        if self.preset_window_heights.is_empty() {
            self.preset_window_heights = Layout::default().preset_window_heights;
        }
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct LayoutPart {
    #[knuffel(child)]
    pub focus_ring: Option<BorderRule>,
    #[knuffel(child)]
    pub border: Option<BorderRule>,
    #[knuffel(child)]
    pub shadow: Option<ShadowRule>,
    #[knuffel(child)]
    pub tab_indicator: Option<TabIndicatorPart>,
    #[knuffel(child)]
    pub insert_hint: Option<InsertHintPart>,
    #[knuffel(child, unwrap(children))]
    pub preset_column_widths: Option<Vec<PresetSize>>,
    #[knuffel(child)]
    pub default_column_width: Option<DefaultPresetSize>,
    #[knuffel(child, unwrap(children))]
    pub preset_window_heights: Option<Vec<PresetSize>>,
    #[knuffel(child, unwrap(argument))]
    pub center_focused_column: Option<CenterFocusedColumn>,
    #[knuffel(child)]
    pub always_center_single_column: Option<Flag>,
    #[knuffel(child)]
    pub disable_carousel: Option<Flag>,
    #[knuffel(child)]
    pub auto_fit_or_center: Option<Flag>,
    #[knuffel(child)]
    pub empty_workspace_above_first: Option<Flag>,
    #[knuffel(child, unwrap(argument, str))]
    pub default_column_display: Option<ColumnDisplay>,
    #[knuffel(child, unwrap(argument))]
    pub gaps: Option<FloatOrInt<0, 65535>>,
    #[knuffel(child)]
    pub struts: Option<Struts>,
    #[knuffel(child)]
    pub background_color: Option<Color>,
    #[knuffel(child)]
    pub terminal_app_ids: Option<TerminalAppIds>,
    #[knuffel(child)]
    pub ultrawide_default_column_width: Option<DefaultPresetSize>,
    #[knuffel(child)]
    pub ultrawide_terminal_column_width: Option<DefaultPresetSize>,
    #[knuffel(child)]
    pub media_player_app_ids: Option<MediaPlayerAppIds>,
    #[knuffel(child)]
    pub media_player_column_width: Option<DefaultPresetSize>,
    #[knuffel(child)]
    pub media_player_ultrawide_column_width: Option<DefaultPresetSize>,
    #[knuffel(child, unwrap(argument, str))]
    pub media_player_open_in_fixed_side: Option<MediaPlayerFixedSide>,
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct TerminalAppIds(#[knuffel(arguments)] pub Vec<String>);

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct MediaPlayerAppIds(#[knuffel(arguments)] pub Vec<String>);

/// Parsed value of `layout { media-player-open-in-fixed-side "..." }`. Accepts `"left"`,
/// `"right"`, or `"none"`/`"off"` (the latter disables the media-player panel routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPlayerFixedSide(pub Option<OpenInFixedSide>);

impl MediaPlayerFixedSide {
    pub fn to_option(self) -> Option<OpenInFixedSide> {
        self.0
    }
}

impl FromStr for MediaPlayerFixedSide {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "left" => Ok(Self(Some(OpenInFixedSide::Left))),
            "right" => Ok(Self(Some(OpenInFixedSide::Right))),
            "none" | "off" => Ok(Self(None)),
            _ => Err(r#"invalid media-player fixed side, can be "left", "right", or "none""#),
        }
    }
}

#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq)]
pub enum PresetSize {
    Proportion(#[knuffel(argument)] f64),
    Fixed(#[knuffel(argument)] i32),
}

impl From<PresetSize> for SizeChange {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(prop) => SizeChange::SetProportion(prop * 100.),
            PresetSize::Fixed(fixed) => SizeChange::SetFixed(fixed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefaultPresetSize(pub Option<PresetSize>);

#[derive(knuffel::Decode, Debug, Default, Clone, Copy, PartialEq)]
pub struct Struts {
    #[knuffel(child, unwrap(argument), default)]
    pub left: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub right: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub top: FloatOrInt<-65535, 65535>,
    #[knuffel(child, unwrap(argument), default)]
    pub bottom: FloatOrInt<-65535, 65535>,
}

#[derive(knuffel::DecodeScalar, Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum CenterFocusedColumn {
    /// Focusing a column will not center the column.
    #[default]
    Never,
    /// The focused column will always be centered.
    Always,
    /// Focusing a column will center it if it doesn't fit on the screen together with the
    /// previously focused column.
    OnOverflow,
}

impl<S> knuffel::Decode<S> for DefaultPresetSize
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        expect_only_children(node, ctx);

        let mut children = node.children();

        if let Some(child) = children.next() {
            if let Some(unwanted_child) = children.next() {
                ctx.emit_error(DecodeError::unexpected(
                    unwanted_child,
                    "node",
                    "expected no more than one child",
                ));
            }
            PresetSize::decode_node(child, ctx).map(Some).map(Self)
        } else {
            Ok(Self(None))
        }
    }
}
