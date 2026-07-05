//! naru config parsing.
//!
//! The config can be constructed from multiple files (includes). To support this, many types are
//! split into two. For example, `Layout` and `LayoutPart` where `Layout` is the final config and
//! `LayoutPart` is one part parsed from one config file.
//!
//! The convention for `Default` impls is to set the initial values before the parsing occurs.
//! Then, parsing will update the values with those parsed from the config.
//!
//! The `Default` values match those from `default-config.kdl` in almost all cases, with a notable
//! exception of `binds {}` and some window rules.

#[macro_use]
extern crate tracing;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use knuffel::errors::DecodeError;
use knuffel::Decode as _;
use miette::{miette, Context as _, IntoDiagnostic as _};

#[macro_use]
pub mod macros;

pub mod animations;
pub mod appearance;
pub mod binds;
pub mod debug;
pub mod error;
pub mod gestures;
pub mod input;
pub mod layer_rule;
pub mod layout;
pub mod misc;
pub mod output;
pub mod recent_windows;
pub mod session_restore;
pub mod utils;
pub mod window_rule;
pub mod workspace;

pub use crate::animations::{Animation, Animations};
pub use crate::appearance::*;
pub use crate::binds::*;
pub use crate::debug::Debug;
pub use crate::error::{ConfigIncludeError, ConfigParseResult};
pub use crate::gestures::Gestures;
pub use crate::input::{Input, ModKey, ScrollMethod, TrackLayout, WarpMouseToFocusMode, Xkb};
pub use crate::layer_rule::LayerRule;
pub use crate::layout::*;
pub use crate::misc::*;
pub use crate::output::{Output, OutputName, Outputs, Position, Vrr};
use crate::recent_windows::RecentWindowsPart;
pub use crate::recent_windows::{MruDirection, MruFilter, MruPreviews, MruScope, RecentWindows};
pub use crate::session_restore::{
    LaunchCommand, LaunchCommandPart, SessionRestore, SessionRestorePart,
};
pub use crate::utils::FloatOrInt;
use crate::utils::{Flag, MergeWith as _};
pub use crate::window_rule::{
    FloatingPosition, PopupsRule, RelativeTo, ResolvedPopupsRules, WindowRule,
};
pub use crate::workspace::{Workspace, WorkspaceLayoutPart};

const RECURSION_LIMIT: u8 = 10;

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub input: Input,
    pub outputs: Outputs,
    pub spawn_at_startup: Vec<SpawnAtStartup>,
    pub spawn_sh_at_startup: Vec<SpawnShAtStartup>,
    pub layout: Layout,
    pub prefer_no_csd: bool,
    /// Enables the stacking move actions (`MoveWindow{Left,Right,Up,Down}Stacked`) and the
    /// alternating new-column / overlap-into-neighbor semantics they implement. When false
    /// (default), those actions parse and dispatch but become no-ops, so the bound keys are
    /// inert until the user opts in by adding `enable-stacking` to their config.
    pub enable_stacking: bool,
    /// Toggles the first-step routing of horizontal cross-column stack moves
    /// (`MoveWindow{Left,Right}Stacked`). When false (default), the moved window is always
    /// inserted as a NEW row in the neighbour column. When true, a single-window source
    /// that IS its whole column instead overlaps onto the neighbour's active tile (the
    /// legacy "merge into neighbour column" behaviour). Vertical within-column moves are
    /// unaffected.
    pub stacking_move_overlap_first: bool,
    pub cursor: Cursor,
    pub screenshot_path: ScreenshotPath,
    pub clipboard: Clipboard,
    pub hotkey_overlay: HotkeyOverlay,
    pub config_notification: ConfigNotification,
    pub appearance: Appearance,
    pub animations: Animations,
    pub blur: Blur,
    pub gestures: Gestures,
    pub overview: Overview,
    pub environment: Environment,
    pub xwayland_satellite: XwaylandSatellite,
    pub window_rules: Vec<WindowRule>,
    pub layer_rules: Vec<LayerRule>,
    pub binds: Binds,
    pub switch_events: SwitchBinds,
    pub debug: Debug,
    pub workspaces: Vec<Workspace>,
    pub recent_windows: RecentWindows,
    pub session_restore: SessionRestore,
}

#[derive(Debug, Clone)]
pub enum ConfigPath {
    /// Explicitly set config path.
    ///
    /// Load the config only from this path, never create it.
    Explicit(PathBuf),

    /// Default config path.
    ///
    /// Prioritize the user path, fallback to the system path, fallback to creating the user path
    /// at compositor startup.
    Regular {
        /// User config path, usually `$XDG_CONFIG_HOME/naru/config.kdl`.
        user_path: PathBuf,
        /// System config path, usually `/etc/naru/config.kdl`.
        system_path: PathBuf,
    },
}

/// Which tier of the load chain produced the running config.
///
/// On startup naru tries the user/system config first, then a previously-saved last-known-good
/// snapshot, and finally the compiled-in defaults. The compositor records which tier won so it
/// can surface a notification telling the user that a fallback is in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// The user (or system) config file loaded successfully.
    User,
    /// The user config failed to load; the last-known-good snapshot was used instead.
    LastGood,
    /// Both the user config and any snapshot failed; compiled-in defaults are in effect.
    BuiltinDefaults,
}

/// Outcome of [`ConfigPath::load_with_fallback`].
pub struct LoadOutcome {
    /// The config that ended up loaded — never an error, since the fallback chain
    /// always terminates at compiled-in defaults.
    pub config: Config,
    /// Which tier of the chain produced [`Self::config`].
    pub source: ConfigSource,
    /// The original error from loading the user config, if it failed. Useful for logging.
    pub user_load_error: Option<miette::Report>,
    /// Path of a freshly created user config (when no config existed before this run).
    pub created_path: Option<PathBuf>,
    /// Includes that the user config pulled in (empty when falling back to snapshot/defaults).
    pub includes: Vec<PathBuf>,
}

/// Returns the path naru uses for the on-disk last-known-good snapshot of the user config.
///
/// The snapshot lives next to the user config, suffixed with `.last-good`, so it inherits
/// the same XDG dir and is trivial for the user to inspect or delete.
pub fn snapshot_path_for(user_path: &Path) -> PathBuf {
    let mut s = user_path.as_os_str().to_owned();
    s.push(".last-good");
    PathBuf::from(s)
}

/// Persists `source` to its `.last-good` sibling so that a future startup can fall back to it
/// if the user config becomes unparseable.
///
/// Best-effort: any error is logged, never propagated. Saving the snapshot must never block
/// the compositor from booting.
///
/// Atomicity is provided by writing to a sibling temp file and then renaming — safe across
/// crashes on POSIX, where rename within the same directory is atomic.
pub fn save_last_good(source: &Path) {
    if let Err(err) = save_last_good_inner(source) {
        warn!("failed to save last-known-good config snapshot: {err}");
    }
}

fn save_last_good_inner(source: &Path) -> std::io::Result<()> {
    let snapshot = snapshot_path_for(source);
    let parent = snapshot.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "snapshot has no parent dir")
    })?;

    let file_name = snapshot
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("config.kdl.last-good");
    let tmp = parent.join(format!(".{file_name}.tmp"));

    // Clean up any stale temp file from a prior crash mid-write.
    let _ = fs::remove_file(&tmp);

    fs::copy(source, &tmp)?;
    fs::rename(&tmp, &snapshot)?;
    Ok(())
}

// Newtypes for putting information into the knuffel context.
struct BasePath(PathBuf);
struct RootBase(PathBuf);
struct Recursion(u8);
#[derive(Default)]
struct Includes(Vec<PathBuf>);
#[derive(Default)]
struct IncludeErrors(Vec<knuffel::Error>);
// Used for recursive include detection.
//
// We don't *need* it because we have a recursion limit, but it makes for nicer error messages.
struct IncludeStack(HashSet<PathBuf>);
struct SawMruBinds(Rc<Cell<bool>>);

// Rather than listing all fields and deriving knuffel::Decode, we implement
// knuffel::DecodeChildren by hand, since we need custom logic for every field anyway: we want to
// merge the values into the config from the context as we go to support the positionality of
// includes. The reason we need this type at all is because knuffel's only entry point that allows
// setting default values on a context is `parse_with_context()` that needs a type to parse.
pub struct ConfigPart;

impl<S> knuffel::DecodeChildren<S> for ConfigPart
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_children(
        nodes: &[knuffel::ast::SpannedNode<S>],
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let _span = tracy_client::span!("decode config file");

        let config = ctx.get::<Rc<RefCell<Config>>>().unwrap().clone();
        let includes = ctx.get::<Rc<RefCell<Includes>>>().unwrap().clone();
        let include_errors = ctx.get::<Rc<RefCell<IncludeErrors>>>().unwrap().clone();
        let recursion = ctx.get::<Recursion>().unwrap().0;
        let saw_mru_binds = ctx.get::<SawMruBinds>().unwrap().0.clone();

        let mut seen = HashSet::new();

        for node in nodes {
            let name = &**node.node_name;

            // Within one config file, splitting sections into multiple parts is not allowed to
            // reduce confusion. The exceptions here aren't multipart; they all add new values.
            if !matches!(
                name,
                "output"
                    | "spawn-at-startup"
                    | "spawn-sh-at-startup"
                    | "window-rule"
                    | "layer-rule"
                    | "workspace"
                    | "include"
            ) && !seen.insert(name)
            {
                ctx.emit_error(DecodeError::unexpected(
                    &node.node_name,
                    "node",
                    format!("duplicate node `{name}`, single node expected"),
                ));
                continue;
            }

            macro_rules! m_merge {
                ($field:ident) => {{
                    let part = knuffel::Decode::decode_node(node, ctx)?;
                    config.borrow_mut().$field.merge_with(&part);
                }};
            }

            macro_rules! m_push {
                ($field:ident) => {{
                    let part = knuffel::Decode::decode_node(node, ctx)?;
                    config.borrow_mut().$field.push(part);
                }};
            }

            match name {
                "input" => m_merge!(input),
                "cursor" => m_merge!(cursor),
                "clipboard" => m_merge!(clipboard),
                "hotkey-overlay" => m_merge!(hotkey_overlay),
                "config-notification" => m_merge!(config_notification),
                "appearance" => m_merge!(appearance),
                "animations" => m_merge!(animations),
                "blur" => m_merge!(blur),
                "gestures" => m_merge!(gestures),
                "overview" => m_merge!(overview),
                "xwayland-satellite" => m_merge!(xwayland_satellite),
                "session-restore" => m_merge!(session_restore),
                "switch-events" => m_merge!(switch_events),
                "debug" => m_merge!(debug),

                // Multipart sections.
                "output" => {
                    let part = Output::decode_node(node, ctx)?;
                    config.borrow_mut().outputs.0.push(part);
                }
                "spawn-at-startup" => m_push!(spawn_at_startup),
                "spawn-sh-at-startup" => m_push!(spawn_sh_at_startup),
                "window-rule" => m_push!(window_rules),
                "layer-rule" => m_push!(layer_rules),
                "workspace" => m_push!(workspaces),

                // Single-part sections.
                "binds" => {
                    let part = Binds::decode_node(node, ctx)?;

                    // We replace conflicting binds, rather than error, to support the use-case
                    // where you import some preconfigured-dots.kdl, then override some binds with
                    // your own.
                    let mut config = config.borrow_mut();
                    let binds = &mut config.binds.0;
                    // Remove existing binds matching any new bind.
                    binds.retain(|bind| !part.0.iter().any(|new| new.key == bind.key));
                    // Add all new binds.
                    binds.extend(part.0);
                }
                "environment" => {
                    let part = Environment::decode_node(node, ctx)?;
                    config.borrow_mut().environment.0.extend(part.0);
                }

                "prefer-no-csd" => {
                    config.borrow_mut().prefer_no_csd = Flag::decode_node(node, ctx)?.0
                }

                "enable-stacking" => {
                    config.borrow_mut().enable_stacking = Flag::decode_node(node, ctx)?.0
                }

                "stacking-move-overlap-first" => {
                    config.borrow_mut().stacking_move_overlap_first =
                        Flag::decode_node(node, ctx)?.0
                }

                "screenshot-path" => {
                    let part = knuffel::Decode::decode_node(node, ctx)?;
                    config.borrow_mut().screenshot_path = part;
                }

                "layout" => {
                    let mut part = LayoutPart::decode_node(node, ctx)?;

                    // Preserve the behavior we'd always had for the border section:
                    // - `layout {}` gives border = off
                    // - `layout { border {} }` gives border = on
                    // - `layout { border { off } }` gives border = off
                    //
                    // This behavior is inconsistent with the rest of the config where adding an
                    // empty section generally doesn't change the outcome. Particularly, shadows
                    // are also disabled by default (like borders), and they always had an `on`
                    // instead of an `off` for this reason, so that writing `layout { shadow {} }`
                    // still results in shadow = off, as it should.
                    //
                    // Unfortunately, the default config has always had wording that heavily
                    // implies that `layout { border {} }` enables the borders. This wording is
                    // sure to be present in a lot of users' configs by now, which we can't change.
                    //
                    // Another way to make things consistent would be to default borders to on.
                    // However, that is annoying because it would mean changing many tests that
                    // rely on borders being off by default. This would also contradict the
                    // intended default borders value (off).
                    //
                    // So, let's just work around the problem here, preserving the original
                    // behavior.
                    if recursion == 0 {
                        if let Some(border) = part.border.as_mut() {
                            if !border.on && !border.off {
                                border.on = true;
                            }
                        }
                    }

                    config.borrow_mut().layout.merge_with(&part);
                }

                "recent-windows" => {
                    let part = RecentWindowsPart::decode_node(node, ctx)?;

                    let mut config = config.borrow_mut();

                    // When an MRU binds section is encountered for the first time, clear out the
                    // default MRU binds.
                    if !saw_mru_binds.get() && part.binds.is_some() {
                        saw_mru_binds.set(true);
                        config.recent_windows.binds.clear();
                    }

                    config.recent_windows.merge_with(&part);
                }

                "include" => {
                    // Parse the path argument
                    let mut iter_args = node.arguments.iter();
                    let path_val = iter_args.next().ok_or_else(|| {
                        DecodeError::missing(
                            node,
                            "additional argument for include path is required",
                        )
                    })?;
                    let path: PathBuf = knuffel::traits::DecodeScalar::decode(path_val, ctx)?;

                    // Check for extra arguments
                    if let Some(val) = iter_args.next() {
                        ctx.emit_error(DecodeError::unexpected(
                            &val.literal,
                            "argument",
                            "unexpected argument",
                        ));
                    }

                    // Parse the optional property
                    let mut optional = false;
                    for (name, val) in &node.properties {
                        match &***name {
                            "optional" => {
                                optional = knuffel::traits::DecodeScalar::decode(val, ctx)?;
                            }
                            name_str => {
                                ctx.emit_error(DecodeError::unexpected(
                                    name,
                                    "property",
                                    format!("unexpected property `{}`", name_str.escape_default()),
                                ));
                            }
                        }
                    }

                    // Check for unexpected children
                    for child in node.children() {
                        ctx.emit_error(DecodeError::unexpected(
                            child,
                            "node",
                            format!("unexpected node `{}`", child.node_name.escape_default()),
                        ));
                    }

                    // We use DecodeError::Missing throughout this block because it results in the
                    // least confusing error messages while still allowing to provide a span.

                    // Expand ~ into the home dir
                    let path = if let Ok(rest) = path.strip_prefix("~") {
                        let Some(home) = std::env::home_dir() else {
                            ctx.emit_error(DecodeError::missing(
                                node,
                                format!("error retrieving home directory to expand {path:?}"),
                            ));
                            continue;
                        };

                        home.join(rest)
                    } else {
                        // Otherwise, use the current include base dir
                        let base = ctx.get::<BasePath>().unwrap();
                        base.0.join(path)
                    };

                    let recursion = ctx.get::<Recursion>().unwrap().0 + 1;
                    if recursion == RECURSION_LIMIT {
                        ctx.emit_error(DecodeError::missing(
                            node,
                            format!(
                                "reached the recursion limit; \
                                 includes cannot be {RECURSION_LIMIT} levels deep"
                            ),
                        ));
                        continue;
                    }

                    let Some(filename) = path.file_name().and_then(OsStr::to_str) else {
                        ctx.emit_error(DecodeError::missing(
                            node,
                            "include path doesn't have a valid file name",
                        ));
                        continue;
                    };
                    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();

                    // Check for recursive include for a nicer error message.
                    let mut include_stack = ctx.get::<IncludeStack>().unwrap().0.clone();
                    if !include_stack.insert(path.to_path_buf()) {
                        ctx.emit_error(DecodeError::missing(
                            node,
                            "recursive include (file includes itself)",
                        ));
                        continue;
                    }

                    // Store even if the include fails to read or parse, so it gets watched.
                    includes.borrow_mut().0.push(path.to_path_buf());

                    match fs::read_to_string(&path) {
                        Ok(text) => {
                            // Try to get filename relative to the root base config folder for
                            // clearer error messages.
                            let root_base = &ctx.get::<RootBase>().unwrap().0;
                            // Failing to strip prefix usually means absolute path; show it in full.
                            let relative_path = path.strip_prefix(root_base).ok().unwrap_or(&path);
                            let filename = relative_path.to_str().unwrap_or(filename);

                            let part = knuffel::parse_with_context::<
                                ConfigPart,
                                knuffel::span::Span,
                                _,
                            >(filename, &text, |ctx| {
                                ctx.set(BasePath(base));
                                ctx.set(RootBase(root_base.clone()));
                                ctx.set(Recursion(recursion));
                                ctx.set(includes.clone());
                                ctx.set(include_errors.clone());
                                ctx.set(IncludeStack(include_stack));
                                ctx.set(SawMruBinds(saw_mru_binds.clone()));
                                ctx.set(config.clone());
                            });

                            match part {
                                Ok(_) => {}
                                Err(err) => {
                                    include_errors.borrow_mut().0.push(err);

                                    ctx.emit_error(DecodeError::missing(
                                        node,
                                        "failed to parse included config",
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            if optional && err.kind() == std::io::ErrorKind::NotFound {
                                // Warn about missing optional includes
                                warn!("optional include not found: {path:?}");
                            } else {
                                // Report all other errors normally
                                ctx.emit_error(DecodeError::missing(
                                    node,
                                    format!("failed to read included config from {path:?}: {err}"),
                                ));
                            }
                        }
                    }
                }

                name => {
                    ctx.emit_error(DecodeError::unexpected(
                        node,
                        "node",
                        format!("unexpected node `{}`", name.escape_default()),
                    ));
                }
            }
        }

        Ok(Self)
    }
}

impl Config {
    pub fn load_default() -> Self {
        let res = Config::parse(
            Path::new("default-config.kdl"),
            include_str!("../../resources/default-config.kdl"),
        );

        // Includes in the default config can break its parsing at runtime.
        assert!(
            res.includes.is_empty(),
            "default config must not have includes",
        );

        res.config.unwrap()
    }

    pub fn load(path: &Path) -> ConfigParseResult<Self, miette::Report> {
        let contents = match fs::read_to_string(path) {
            Ok(x) => x,
            Err(err) => {
                return ConfigParseResult::from_err(
                    miette!(err).context(format!("error reading {path:?}")),
                );
            }
        };

        Self::parse(path, &contents).map_config_res(|res| {
            let config = res.context("error parsing")?;
            debug!("loaded config from {path:?}");
            Ok(config)
        })
    }

    pub fn parse(path: &Path, text: &str) -> ConfigParseResult<Self, ConfigIncludeError> {
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("config.kdl");

        let config = Rc::new(RefCell::new(Config::default()));
        let includes = Rc::new(RefCell::new(Includes(Vec::new())));
        let include_errors = Rc::new(RefCell::new(IncludeErrors(Vec::new())));
        let include_stack = HashSet::from([path.to_path_buf()]);

        let part = knuffel::parse_with_context::<ConfigPart, knuffel::span::Span, _>(
            filename,
            text,
            |ctx| {
                ctx.set(BasePath(base.clone()));
                ctx.set(RootBase(base));
                ctx.set(Recursion(0));
                ctx.set(includes.clone());
                ctx.set(include_errors.clone());
                ctx.set(IncludeStack(include_stack));
                ctx.set(SawMruBinds(Rc::new(Cell::new(false))));
                ctx.set(config.clone());
            },
        );

        let includes = includes.take().0;
        let include_errors = include_errors.take().0;
        let config = part
            .map(|_| config.take())
            .map_err(move |err| ConfigIncludeError {
                main: err,
                includes: include_errors,
            });

        ConfigParseResult { config, includes }
    }

    pub fn parse_mem(text: &str) -> Result<Self, ConfigIncludeError> {
        Self::parse(Path::new("config.kdl"), text).config
    }
}

impl ConfigPath {
    /// Loads the config, returns an error if it doesn't exist.
    pub fn load(&self) -> ConfigParseResult<Config, miette::Report> {
        let _span = tracy_client::span!("ConfigPath::load");

        self.load_inner(|user_path, system_path| {
            Err(miette!(
                "no config file found; create one at {user_path:?} or {system_path:?}",
            ))
        })
        .map_config_res(|res| res.context("error loading config"))
    }

    /// Loads the config, or creates it if it doesn't exist.
    ///
    /// Returns a tuple containing the path that was created, if any, and the loaded config.
    ///
    /// If the config was created, but for some reason could not be read afterwards,
    /// this may return `(Some(_), Err(_))`.
    pub fn load_or_create(&self) -> (Option<&Path>, ConfigParseResult<Config, miette::Report>) {
        let _span = tracy_client::span!("ConfigPath::load_or_create");

        let mut created_at = None;

        let result = self
            .load_inner(|user_path, _| {
                Self::create(user_path, &mut created_at)
                    .map(|()| user_path)
                    .with_context(|| format!("error creating config at {user_path:?}"))
            })
            .map_config_res(|res| res.context("error loading config"));

        (created_at, result)
    }

    /// Returns the user-facing config path, if one is associated with this `ConfigPath`.
    ///
    /// Both `Explicit` and `Regular` variants have one; only `Explicit` skips the system fallback.
    /// This is the path used as the source for the last-known-good snapshot.
    pub fn user_path(&self) -> Option<&Path> {
        match self {
            ConfigPath::Explicit(p) => Some(p.as_path()),
            ConfigPath::Regular { user_path, .. } => Some(user_path.as_path()),
        }
    }

    /// Returns the on-disk path naru would use for the last-known-good snapshot, if any.
    pub fn last_good_path(&self) -> Option<PathBuf> {
        self.user_path().map(snapshot_path_for)
    }

    /// Loads the config with a multi-tier fallback chain.
    ///
    /// 1. Try the user/system config (creating one from defaults if neither exists).
    /// 2. If that fails to parse, try `<user>.last-good` — a snapshot saved by a previous
    ///    successful startup.
    /// 3. If that also fails or doesn't exist, return compiled-in defaults.
    ///
    /// The returned [`LoadOutcome`] always carries a usable [`Config`], so callers don't need
    /// to handle the "everything is broken" case — keybindings will keep working even when
    /// the user has saved a totally broken config.
    pub fn load_with_fallback(&self) -> LoadOutcome {
        let _span = tracy_client::span!("ConfigPath::load_with_fallback");

        let (created_at, parse_result) = self.load_or_create();
        let created_path = created_at.map(Path::to_path_buf);
        let user_includes = parse_result.includes;

        match parse_result.config {
            Ok(config) => LoadOutcome {
                config,
                source: ConfigSource::User,
                user_load_error: None,
                created_path,
                includes: user_includes,
            },
            Err(err) => {
                // Try the last-known-good snapshot before falling back to compiled defaults.
                if let Some(snapshot) = self.last_good_path() {
                    if snapshot.exists() {
                        let res = Config::load(&snapshot);
                        match res.config {
                            Ok(config) => {
                                warn!(
                                    "user config failed to load; using last-known-good \
                                     snapshot at {snapshot:?}",
                                );
                                return LoadOutcome {
                                    config,
                                    source: ConfigSource::LastGood,
                                    user_load_error: Some(err),
                                    created_path,
                                    includes: res.includes,
                                };
                            }
                            Err(snap_err) => {
                                warn!(
                                    "last-known-good snapshot at {snapshot:?} also failed \
                                     to parse: {snap_err:?}",
                                );
                            }
                        }
                    }
                }

                warn!("falling back to built-in defaults so the compositor can start");
                LoadOutcome {
                    config: Config::load_default(),
                    source: ConfigSource::BuiltinDefaults,
                    user_load_error: Some(err),
                    created_path,
                    includes: Vec::new(),
                }
            }
        }
    }

    fn load_inner<'a>(
        &'a self,
        maybe_create: impl FnOnce(&'a Path, &'a Path) -> miette::Result<&'a Path>,
    ) -> ConfigParseResult<Config, miette::Report> {
        let path = match self {
            ConfigPath::Explicit(path) => path.as_path(),
            ConfigPath::Regular {
                user_path,
                system_path,
            } => {
                if user_path.exists() {
                    user_path.as_path()
                } else if system_path.exists() {
                    system_path.as_path()
                } else {
                    match maybe_create(user_path.as_path(), system_path.as_path()) {
                        Ok(x) => x,
                        Err(err) => return ConfigParseResult::from_err(miette!(err)),
                    }
                }
            }
        };
        Config::load(path)
    }

    fn create<'a>(path: &'a Path, created_at: &mut Option<&'a Path>) -> miette::Result<()> {
        if let Some(default_parent) = path.parent() {
            fs::create_dir_all(default_parent)
                .into_diagnostic()
                .with_context(|| format!("error creating config directory {default_parent:?}"))?;
        }

        // Create the config and fill it with the default config if it doesn't exist.
        let mut new_file = match File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            res => res,
        }
        .into_diagnostic()
        .with_context(|| format!("error opening config file at {path:?}"))?;

        *created_at = Some(path);

        let default = include_bytes!("../../resources/default-config.kdl");

        new_file
            .write_all(default)
            .into_diagnostic()
            .with_context(|| format!("error writing default config to {path:?}"))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use insta::{assert_debug_snapshot, assert_snapshot};
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn can_create_default_config() {
        let _ = Config::load_default();
    }

    #[test]
    fn parse_media_player_layout() {
        let config = do_parse(
            r##"
            layout {
                media-player-app-ids "mpv" "org.videolan.VLC"
                media-player-column-width { proportion 0.25; }
                media-player-ultrawide-column-width { proportion 0.15; }
                media-player-open-in-fixed-side "right"
            }
            "##,
        );
        assert_eq!(
            config.layout.media_player_app_ids,
            vec!["mpv".to_owned(), "org.videolan.VLC".to_owned()]
        );
        assert_eq!(
            config.layout.media_player_column_width,
            PresetSize::Proportion(0.25)
        );
        assert_eq!(
            config.layout.media_player_ultrawide_column_width,
            PresetSize::Proportion(0.15)
        );
        assert_eq!(
            config.layout.media_player_open_in_fixed_side,
            Some(naru_ipc::OpenInFixedSide::Right)
        );
    }

    #[test]
    fn media_player_fixed_side_none_disables_routing() {
        let config = do_parse(
            r##"
            layout {
                media-player-open-in-fixed-side "none"
            }
            "##,
        );
        assert_eq!(config.layout.media_player_open_in_fixed_side, None);
    }

    #[test]
    fn media_player_defaults() {
        let config = Config::parse_mem("").unwrap();
        assert!(config.layout.media_player_app_ids.is_empty());
        assert_eq!(
            config.layout.media_player_column_width,
            PresetSize::Proportion(1. / 3.)
        );
        assert_eq!(
            config.layout.media_player_ultrawide_column_width,
            PresetSize::Proportion(1. / 5.)
        );
        assert_eq!(
            config.layout.media_player_open_in_fixed_side,
            Some(naru_ipc::OpenInFixedSide::Left)
        );
    }

    #[test]
    fn default_repeat_params() {
        let config = Config::parse_mem("").unwrap();
        assert_eq!(config.input.keyboard.repeat_delay, 600);
        assert_eq!(config.input.keyboard.repeat_rate, 25);
    }

    #[track_caller]
    fn do_parse(text: &str) -> Config {
        Config::parse_mem(text)
            .map_err(miette::Report::new)
            .unwrap()
    }

    #[test]
    fn parse() {
        let parsed = do_parse(
            r##"
            input {
                keyboard {
                    repeat-delay 600
                    repeat-rate 25
                    track-layout "window"
                    xkb {
                        layout "us,ru"
                        options "grp:win_space_toggle"
                    }
                }

                touchpad {
                    tap
                    dwt
                    dwtp
                    drag true
                    click-method "clickfinger"
                    accel-speed 0.2
                    accel-profile "flat"
                    scroll-method "two-finger"
                    scroll-button 272
                    scroll-button-lock
                    tap-button-map "left-middle-right"
                    disabled-on-external-mouse
                    scroll-factor 0.9
                }

                mouse {
                    natural-scroll
                    accel-speed 0.4
                    accel-profile "flat"
                    scroll-method "no-scroll"
                    scroll-button 273
                    middle-emulation
                    scroll-factor 0.2
                }

                trackpoint {
                    off
                    natural-scroll
                    accel-speed 0.0
                    accel-profile "flat"
                    scroll-method "on-button-down"
                    scroll-button 274
                }

                trackball {
                    off
                    natural-scroll
                    accel-speed 0.0
                    accel-profile "flat"
                    scroll-method "edge"
                    scroll-button 275
                    scroll-button-lock
                    left-handed
                    middle-emulation
                }

                tablet {
                    map-to-output "eDP-1"
                    map-to-focused-output
                    map-to-focused-window
                    calibration-matrix 1.0 2.0 3.0 \
                                       4.0 5.0 6.0
                }

                touch {
                    map-to-output "eDP-1"
                }

                disable-power-key-handling

                warp-mouse-to-focus
                focus-follows-mouse
                workspace-auto-back-and-forth

                mod-key "Mod5"
                mod-key-nested "Super"
            }

            output "eDP-1" {
                focus-at-startup
                scale 2
                transform "flipped-90"
                position x=10 y=20
                mode "1920x1080@144"
                variable-refresh-rate on-demand=true
                background-color "rgba(25, 25, 102, 1.0)"
                hot-corners {
                    off
                    top-left
                    top-right
                    bottom-left
                    bottom-right
                }
            }

            output "eDP-2" {
                mode custom=true "1920x1080@144"
            }

            output "eDP-3" {
                modeline 173.00  1920 2048 2248 2576  1080 1083 1088 1120 "-hsync" "+vsync"
            }

            layout {
                focus-ring {
                    width 5
                    active-color 0 100 200 255
                    inactive-color 255 200 100 0
                    active-gradient from="rgba(10, 20, 30, 1.0)" to="#0080ffff" relative-to="workspace-view"
                }

                border {
                    width 3
                    inactive-color "rgba(255, 200, 100, 0.0)"
                }

                shadow {
                    offset x=10 y=-20
                }

                tab-indicator {
                    width 10
                    position "top"
                }

                preset-column-widths {
                    proportion 0.25
                    proportion 0.5
                    fixed 960
                    fixed 1280
                }

                preset-window-heights {
                    proportion 0.25
                    proportion 0.5
                    fixed 960
                    fixed 1280
                }

                default-column-width { proportion 0.25; }

                gaps 8

                struts {
                    left 1
                    right 2
                    top 3
                }

                center-focused-column "on-overflow"

                default-column-display "tabbed"

                insert-hint {
                    color "rgb(255, 200, 127)"
                    gradient from="rgba(10, 20, 30, 1.0)" to="#0080ffff" relative-to="workspace-view"
                }
            }

            spawn-at-startup "alacritty" "-e" "fish"
            spawn-sh-at-startup "qs -c ~/source/qs/MyAwesomeShell"

            prefer-no-csd

            cursor {
                xcursor-theme "breeze_cursors"
                xcursor-size 16
                hide-when-typing
                hide-after-inactive-ms 3000
            }

            screenshot-path "~/Screenshots/screenshot.png"

            clipboard {
                disable-primary
            }

            hotkey-overlay {
                skip-at-startup
            }

            animations {
                slowdown 2.0

                workspace-switch {
                    spring damping-ratio=1.0 stiffness=1000 epsilon=0.0001
                }

                horizontal-view-movement {
                    duration-ms 100
                    curve "ease-out-expo"
                }

                window-open { off; }

                window-close {
                    curve "cubic-bezier" 0.05 0.7 0.1 1  
                }

                recent-windows-close {
                    off
                }
            }

            gestures {
                dnd-edge-view-scroll {
                    trigger-width 10
                    max-speed 50
                }
            }

            environment {
                QT_QPA_PLATFORM "wayland"
                DISPLAY null
            }

            window-rule {
                match app-id=".*alacritty"
                exclude title="~"
                exclude is-active=true is-focused=false

                open-on-output "eDP-1"
                open-maximized true
                open-fullscreen false
                open-floating false
                open-focused true
                default-window-height { fixed 500; }
                default-column-display "tabbed"
                default-floating-position x=100 y=-200 relative-to="bottom-left"

                focus-ring {
                    off
                    width 3
                }

                border {
                    on
                    width 8.5
                }

                tab-indicator {
                    active-color "#f00"
                }
            }

            layer-rule {
                match namespace="^notifications$"
                block-out-from "screencast"
            }

            binds {
                Mod+Escape hotkey-overlay-title="Inhibit" { toggle-keyboard-shortcuts-inhibit; }
                Mod+Shift+Escape allow-inhibiting=true { toggle-keyboard-shortcuts-inhibit; }
                Mod+T allow-when-locked=true { spawn "alacritty"; }
                Mod+Q hotkey-overlay-title=null { close-window; }
                Mod+Shift+H { focus-monitor-left; }
                Mod+Shift+O { focus-monitor "eDP-1"; }
                Mod+Ctrl+Shift+L { move-window-to-monitor-right; }
                Mod+Ctrl+Alt+O { move-window-to-monitor "eDP-1"; }
                Mod+Ctrl+Alt+P { move-column-to-monitor "DP-1"; }
                Mod+Comma { consume-window-into-column; }
                Mod+1 { focus-workspace 1; }
                Mod+Shift+1 { focus-workspace "workspace-1"; }
                Mod+Shift+E allow-inhibiting=false { quit skip-confirmation=true; }
                Mod+WheelScrollDown cooldown-ms=150 { focus-workspace-down; }
                Super+Alt+S allow-when-locked=true { spawn-sh "pkill orca || exec orca"; }
            }

            switch-events {
                tablet-mode-on { spawn "bash" "-c" "gsettings set org.gnome.desktop.a11y.applications screen-keyboard-enabled true"; }
                tablet-mode-off { spawn "bash" "-c" "gsettings set org.gnome.desktop.a11y.applications screen-keyboard-enabled false"; }
            }

            debug {
                render-drm-device "/dev/dri/renderD129"
                ignore-drm-device "/dev/dri/renderD128"
                ignore-drm-device "/dev/dri/renderD130"
            }

            workspace "workspace-1" {
                open-on-output "eDP-1"
            }
            workspace "workspace-2"
            workspace "workspace-3"

            recent-windows {
                off

                highlight {
                    padding 15
                    active-color "#00ff00"
                }

                previews {
                    max-height 960
                }

                binds {
                    Alt+Tab { next-window; }
                    Alt+grave { next-window filter="app-id"; }
                    Super+Tab { next-window scope="output"; }
                }
            }
            "##,
        );

        assert_debug_snapshot!(parsed, @r#"
        Config {
            input: Input {
                keyboard: Keyboard {
                    xkb: Xkb {
                        rules: "",
                        model: "",
                        layout: "us,ru",
                        variant: "",
                        options: Some(
                            "grp:win_space_toggle",
                        ),
                        file: None,
                    },
                    repeat_delay: 600,
                    repeat_rate: 25,
                    track_layout: Window,
                    numlock: false,
                },
                touchpad: Touchpad {
                    off: false,
                    tap: true,
                    dwt: true,
                    dwtp: true,
                    drag: Some(
                        true,
                    ),
                    drag_lock: false,
                    natural_scroll: false,
                    click_method: Some(
                        Clickfinger,
                    ),
                    accel_speed: FloatOrInt(
                        0.2,
                    ),
                    accel_profile: Some(
                        Flat,
                    ),
                    scroll_method: Some(
                        TwoFinger,
                    ),
                    scroll_button: Some(
                        272,
                    ),
                    scroll_button_lock: true,
                    tap_button_map: Some(
                        LeftMiddleRight,
                    ),
                    left_handed: false,
                    disabled_on_external_mouse: true,
                    middle_emulation: false,
                    scroll_factor: Some(
                        ScrollFactor {
                            base: Some(
                                FloatOrInt(
                                    0.9,
                                ),
                            ),
                            horizontal: None,
                            vertical: None,
                        },
                    ),
                },
                mouse: Mouse {
                    off: false,
                    natural_scroll: true,
                    accel_speed: FloatOrInt(
                        0.4,
                    ),
                    accel_profile: Some(
                        Flat,
                    ),
                    scroll_method: Some(
                        NoScroll,
                    ),
                    scroll_button: Some(
                        273,
                    ),
                    scroll_button_lock: false,
                    left_handed: false,
                    middle_emulation: true,
                    scroll_factor: Some(
                        ScrollFactor {
                            base: Some(
                                FloatOrInt(
                                    0.2,
                                ),
                            ),
                            horizontal: None,
                            vertical: None,
                        },
                    ),
                },
                trackpoint: Trackpoint {
                    off: true,
                    natural_scroll: true,
                    accel_speed: FloatOrInt(
                        0.0,
                    ),
                    accel_profile: Some(
                        Flat,
                    ),
                    scroll_method: Some(
                        OnButtonDown,
                    ),
                    scroll_button: Some(
                        274,
                    ),
                    scroll_button_lock: false,
                    left_handed: false,
                    middle_emulation: false,
                },
                trackball: Trackball {
                    off: true,
                    natural_scroll: true,
                    accel_speed: FloatOrInt(
                        0.0,
                    ),
                    accel_profile: Some(
                        Flat,
                    ),
                    scroll_method: Some(
                        Edge,
                    ),
                    scroll_button: Some(
                        275,
                    ),
                    scroll_button_lock: true,
                    left_handed: true,
                    middle_emulation: true,
                },
                tablet: Tablet {
                    off: false,
                    calibration_matrix: Some(
                        [
                            1.0,
                            2.0,
                            3.0,
                            4.0,
                            5.0,
                            6.0,
                        ],
                    ),
                    map_to_output: Some(
                        "eDP-1",
                    ),
                    map_to_focused_output: true,
                    map_to_focused_window: true,
                    left_handed: false,
                },
                touch: Touch {
                    off: false,
                    calibration_matrix: None,
                    map_to_output: Some(
                        "eDP-1",
                    ),
                },
                disable_power_key_handling: true,
                warp_mouse_to_focus: Some(
                    WarpMouseToFocus {
                        mode: None,
                    },
                ),
                focus_follows_mouse: Some(
                    FocusFollowsMouse {
                        max_scroll_amount: None,
                    },
                ),
                workspace_auto_back_and_forth: true,
                mod_key: Some(
                    IsoLevel3Shift,
                ),
                mod_key_nested: Some(
                    Super,
                ),
            },
            outputs: Outputs(
                [
                    Output {
                        off: false,
                        name: "eDP-1",
                        scale: Some(
                            FloatOrInt(
                                2.0,
                            ),
                        ),
                        transform: Flipped90,
                        position: Some(
                            Position {
                                x: 10,
                                y: 20,
                            },
                        ),
                        mode: Some(
                            Mode {
                                custom: false,
                                mode: ConfiguredMode {
                                    width: 1920,
                                    height: 1080,
                                    refresh: Some(
                                        144.0,
                                    ),
                                },
                            },
                        ),
                        modeline: None,
                        variable_refresh_rate: Some(
                            Vrr {
                                on_demand: true,
                            },
                        ),
                        focus_at_startup: true,
                        background_color: Some(
                            Color {
                                r: 0.09803922,
                                g: 0.09803922,
                                b: 0.4,
                                a: 1.0,
                            },
                        ),
                        backdrop_color: None,
                        hot_corners: Some(
                            HotCorners {
                                off: true,
                                top_left: true,
                                top_right: true,
                                bottom_left: true,
                                bottom_right: true,
                            },
                        ),
                        layout: None,
                    },
                    Output {
                        off: false,
                        name: "eDP-2",
                        scale: None,
                        transform: Normal,
                        position: None,
                        mode: Some(
                            Mode {
                                custom: true,
                                mode: ConfiguredMode {
                                    width: 1920,
                                    height: 1080,
                                    refresh: Some(
                                        144.0,
                                    ),
                                },
                            },
                        ),
                        modeline: None,
                        variable_refresh_rate: None,
                        focus_at_startup: false,
                        background_color: None,
                        backdrop_color: None,
                        hot_corners: None,
                        layout: None,
                    },
                    Output {
                        off: false,
                        name: "eDP-3",
                        scale: None,
                        transform: Normal,
                        position: None,
                        mode: None,
                        modeline: Some(
                            Modeline {
                                clock: 173.0,
                                hdisplay: 1920,
                                hsync_start: 2048,
                                hsync_end: 2248,
                                htotal: 2576,
                                vdisplay: 1080,
                                vsync_start: 1083,
                                vsync_end: 1088,
                                vtotal: 1120,
                                hsync_polarity: NHSync,
                                vsync_polarity: PVSync,
                            },
                        ),
                        variable_refresh_rate: None,
                        focus_at_startup: false,
                        background_color: None,
                        backdrop_color: None,
                        hot_corners: None,
                        layout: None,
                    },
                ],
            ),
            spawn_at_startup: [
                SpawnAtStartup {
                    command: [
                        "alacritty",
                        "-e",
                        "fish",
                    ],
                },
            ],
            spawn_sh_at_startup: [
                SpawnShAtStartup {
                    command: "qs -c ~/source/qs/MyAwesomeShell",
                },
            ],
            layout: Layout {
                focus_ring: FocusRing {
                    off: false,
                    width: 5.0,
                    active_color: Color {
                        r: 0.0,
                        g: 0.39215687,
                        b: 0.78431374,
                        a: 1.0,
                    },
                    inactive_color: Color {
                        r: 1.0,
                        g: 0.78431374,
                        b: 0.39215687,
                        a: 0.0,
                    },
                    urgent_color: Color {
                        r: 0.60784316,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    active_gradient: Some(
                        Gradient {
                            from: Color {
                                r: 0.039215688,
                                g: 0.078431375,
                                b: 0.11764706,
                                a: 1.0,
                            },
                            to: Color {
                                r: 0.0,
                                g: 0.5019608,
                                b: 1.0,
                                a: 1.0,
                            },
                            angle: 180,
                            relative_to: WorkspaceView,
                            in_: GradientInterpolation {
                                color_space: Srgb,
                                hue_interpolation: Shorter,
                            },
                        },
                    ),
                    inactive_gradient: None,
                    urgent_gradient: None,
                },
                border: Border {
                    off: false,
                    width: 3.0,
                    active_color: Color {
                        r: 1.0,
                        g: 0.78431374,
                        b: 0.49803922,
                        a: 1.0,
                    },
                    inactive_color: Color {
                        r: 1.0,
                        g: 0.78431374,
                        b: 0.39215687,
                        a: 0.0,
                    },
                    urgent_color: Color {
                        r: 0.60784316,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    active_gradient: None,
                    inactive_gradient: None,
                    urgent_gradient: None,
                },
                shadow: Shadow {
                    on: false,
                    offset: ShadowOffset {
                        x: FloatOrInt(
                            10.0,
                        ),
                        y: FloatOrInt(
                            -20.0,
                        ),
                    },
                    softness: 30.0,
                    spread: 5.0,
                    draw_behind_window: false,
                    color: Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.46666667,
                    },
                    inactive_color: None,
                },
                tab_indicator: TabIndicator {
                    off: false,
                    hide_when_single_tab: false,
                    place_within_column: false,
                    gap: 5.0,
                    width: 10.0,
                    length: TabIndicatorLength {
                        total_proportion: Some(
                            0.5,
                        ),
                    },
                    position: Top,
                    gaps_between_tabs: 0.0,
                    corner_radius: 0.0,
                    active_color: None,
                    inactive_color: None,
                    urgent_color: None,
                    active_gradient: None,
                    inactive_gradient: None,
                    urgent_gradient: None,
                },
                insert_hint: InsertHint {
                    off: false,
                    color: Color {
                        r: 1.0,
                        g: 0.78431374,
                        b: 0.49803922,
                        a: 1.0,
                    },
                    gradient: Some(
                        Gradient {
                            from: Color {
                                r: 0.039215688,
                                g: 0.078431375,
                                b: 0.11764706,
                                a: 1.0,
                            },
                            to: Color {
                                r: 0.0,
                                g: 0.5019608,
                                b: 1.0,
                                a: 1.0,
                            },
                            angle: 180,
                            relative_to: WorkspaceView,
                            in_: GradientInterpolation {
                                color_space: Srgb,
                                hue_interpolation: Shorter,
                            },
                        },
                    ),
                },
                preset_column_widths: [
                    Proportion(
                        0.25,
                    ),
                    Proportion(
                        0.5,
                    ),
                    Fixed(
                        960,
                    ),
                    Fixed(
                        1280,
                    ),
                ],
                default_column_width: Some(
                    Proportion(
                        0.25,
                    ),
                ),
                preset_window_heights: [
                    Proportion(
                        0.25,
                    ),
                    Proportion(
                        0.5,
                    ),
                    Fixed(
                        960,
                    ),
                    Fixed(
                        1280,
                    ),
                ],
                center_focused_column: OnOverflow,
                always_center_single_column: false,
                disable_carousel: false,
                auto_fit_or_center: false,
                empty_workspace_above_first: false,
                default_column_display: Tabbed,
                gaps: 8.0,
                struts: Struts {
                    left: FloatOrInt(
                        1.0,
                    ),
                    right: FloatOrInt(
                        2.0,
                    ),
                    top: FloatOrInt(
                        3.0,
                    ),
                    bottom: FloatOrInt(
                        0.0,
                    ),
                },
                background_color: Color {
                    r: 0.25,
                    g: 0.25,
                    b: 0.25,
                    a: 1.0,
                },
                terminal_app_ids: [],
                ultrawide_default_column_width: Proportion(
                    0.3333333333333333,
                ),
                ultrawide_terminal_column_width: Proportion(
                    0.2,
                ),
                media_player_app_ids: [],
                media_player_column_width: Proportion(
                    0.3333333333333333,
                ),
                media_player_ultrawide_column_width: Proportion(
                    0.2,
                ),
                media_player_open_in_fixed_side: Some(
                    Left,
                ),
            },
            prefer_no_csd: true,
            enable_stacking: false,
            stacking_move_overlap_first: false,
            cursor: Cursor {
                xcursor_theme: "breeze_cursors",
                xcursor_size: 16,
                hide_when_typing: true,
                hide_after_inactive_ms: Some(
                    3000,
                ),
            },
            screenshot_path: ScreenshotPath(
                Some(
                    "~/Screenshots/screenshot.png",
                ),
            ),
            clipboard: Clipboard {
                disable_primary: true,
            },
            hotkey_overlay: HotkeyOverlay {
                skip_at_startup: true,
                hide_not_bound: false,
            },
            config_notification: ConfigNotification {
                disable_failed: false,
            },
            appearance: Appearance {
                font_size: 13,
            },
            animations: Animations {
                off: false,
                slowdown: 2.0,
                workspace_switch: WorkspaceSwitchAnim(
                    Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 1.0,
                                stiffness: 1000,
                                epsilon: 0.0001,
                            },
                        ),
                    },
                ),
                window_open: WindowOpenAnim {
                    anim: Animation {
                        off: true,
                        kind: Easing(
                            EasingParams {
                                duration_ms: 150,
                                curve: EaseOutExpo,
                            },
                        ),
                    },
                    custom_shader: None,
                },
                window_close: WindowCloseAnim {
                    anim: Animation {
                        off: false,
                        kind: Easing(
                            EasingParams {
                                duration_ms: 150,
                                curve: CubicBezier(
                                    0.05,
                                    0.7,
                                    0.1,
                                    1.0,
                                ),
                            },
                        ),
                    },
                    custom_shader: None,
                },
                horizontal_view_movement: HorizontalViewMovementAnim(
                    Animation {
                        off: false,
                        kind: Easing(
                            EasingParams {
                                duration_ms: 100,
                                curve: EaseOutExpo,
                            },
                        ),
                    },
                ),
                window_movement: WindowMovementAnim(
                    Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 1.0,
                                stiffness: 800,
                                epsilon: 0.0001,
                            },
                        ),
                    },
                ),
                window_resize: WindowResizeAnim {
                    anim: Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 1.0,
                                stiffness: 800,
                                epsilon: 0.0001,
                            },
                        ),
                    },
                    custom_shader: None,
                },
                config_notification_open_close: ConfigNotificationOpenCloseAnim(
                    Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 0.6,
                                stiffness: 1000,
                                epsilon: 0.001,
                            },
                        ),
                    },
                ),
                exit_confirmation_open_close: ExitConfirmationOpenCloseAnim(
                    Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 0.6,
                                stiffness: 500,
                                epsilon: 0.01,
                            },
                        ),
                    },
                ),
                screenshot_ui_open: ScreenshotUiOpenAnim(
                    Animation {
                        off: false,
                        kind: Easing(
                            EasingParams {
                                duration_ms: 200,
                                curve: EaseOutQuad,
                            },
                        ),
                    },
                ),
                overview_open_close: OverviewOpenCloseAnim(
                    Animation {
                        off: false,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 1.0,
                                stiffness: 800,
                                epsilon: 0.0001,
                            },
                        ),
                    },
                ),
                recent_windows_close: RecentWindowsCloseAnim(
                    Animation {
                        off: true,
                        kind: Spring(
                            SpringParams {
                                damping_ratio: 1.0,
                                stiffness: 800,
                                epsilon: 0.001,
                            },
                        ),
                    },
                ),
            },
            blur: Blur {
                off: false,
                passes: 3,
                offset: 3.0,
                noise: 0.02,
                saturation: 1.5,
            },
            gestures: Gestures {
                dnd_edge_view_scroll: DndEdgeViewScroll {
                    trigger_width: 10.0,
                    delay_ms: 100,
                    max_speed: 50.0,
                },
                dnd_edge_workspace_switch: DndEdgeWorkspaceSwitch {
                    trigger_height: 50.0,
                    delay_ms: 100,
                    max_speed: 1500.0,
                },
                hot_corners: HotCorners {
                    off: false,
                    top_left: false,
                    top_right: false,
                    bottom_left: false,
                    bottom_right: false,
                },
            },
            overview: Overview {
                zoom: 0.5,
                backdrop_color: Color {
                    r: 0.15,
                    g: 0.15,
                    b: 0.15,
                    a: 1.0,
                },
                workspace_shadow: WorkspaceShadow {
                    off: false,
                    offset: ShadowOffset {
                        x: FloatOrInt(
                            0.0,
                        ),
                        y: FloatOrInt(
                            10.0,
                        ),
                    },
                    softness: 40.0,
                    spread: 10.0,
                    color: Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.3137255,
                    },
                },
            },
            environment: Environment(
                [
                    EnvironmentVariable {
                        name: "QT_QPA_PLATFORM",
                        value: Some(
                            "wayland",
                        ),
                    },
                    EnvironmentVariable {
                        name: "DISPLAY",
                        value: None,
                    },
                ],
            ),
            xwayland_satellite: XwaylandSatellite {
                off: false,
                path: "xwayland-satellite",
            },
            window_rules: [
                WindowRule {
                    matches: [
                        Match {
                            app_id: Some(
                                RegexEq(
                                    Regex(
                                        ".*alacritty",
                                    ),
                                ),
                            ),
                            title: None,
                            is_active: None,
                            is_focused: None,
                            is_active_in_column: None,
                            is_floating: None,
                            is_window_cast_target: None,
                            is_urgent: None,
                            at_startup: None,
                        },
                    ],
                    excludes: [
                        Match {
                            app_id: None,
                            title: Some(
                                RegexEq(
                                    Regex(
                                        "~",
                                    ),
                                ),
                            ),
                            is_active: None,
                            is_focused: None,
                            is_active_in_column: None,
                            is_floating: None,
                            is_window_cast_target: None,
                            is_urgent: None,
                            at_startup: None,
                        },
                        Match {
                            app_id: None,
                            title: None,
                            is_active: Some(
                                true,
                            ),
                            is_focused: Some(
                                false,
                            ),
                            is_active_in_column: None,
                            is_floating: None,
                            is_window_cast_target: None,
                            is_urgent: None,
                            at_startup: None,
                        },
                    ],
                    default_column_width: None,
                    default_window_height: Some(
                        DefaultPresetSize(
                            Some(
                                Fixed(
                                    500,
                                ),
                            ),
                        ),
                    ),
                    open_on_output: Some(
                        "eDP-1",
                    ),
                    open_on_workspace: None,
                    open_maximized: Some(
                        true,
                    ),
                    open_maximized_to_edges: None,
                    open_fullscreen: Some(
                        false,
                    ),
                    open_floating: Some(
                        false,
                    ),
                    open_focused: Some(
                        true,
                    ),
                    open_in_fixed_side: None,
                    min_width: None,
                    min_height: None,
                    max_width: None,
                    max_height: None,
                    focus_ring: BorderRule {
                        off: true,
                        on: false,
                        width: Some(
                            FloatOrInt(
                                3.0,
                            ),
                        ),
                        active_color: None,
                        inactive_color: None,
                        urgent_color: None,
                        active_gradient: None,
                        inactive_gradient: None,
                        urgent_gradient: None,
                    },
                    border: BorderRule {
                        off: false,
                        on: true,
                        width: Some(
                            FloatOrInt(
                                8.5,
                            ),
                        ),
                        active_color: None,
                        inactive_color: None,
                        urgent_color: None,
                        active_gradient: None,
                        inactive_gradient: None,
                        urgent_gradient: None,
                    },
                    shadow: ShadowRule {
                        off: false,
                        on: false,
                        offset: None,
                        softness: None,
                        spread: None,
                        draw_behind_window: None,
                        color: None,
                        inactive_color: None,
                    },
                    tab_indicator: TabIndicatorRule {
                        active_color: Some(
                            Color {
                                r: 1.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            },
                        ),
                        inactive_color: None,
                        urgent_color: None,
                        active_gradient: None,
                        inactive_gradient: None,
                        urgent_gradient: None,
                    },
                    draw_border_with_background: None,
                    opacity: None,
                    geometry_corner_radius: None,
                    clip_to_geometry: None,
                    baba_is_float: None,
                    block_out_from: None,
                    variable_refresh_rate: None,
                    default_column_display: Some(
                        Tabbed,
                    ),
                    default_floating_position: Some(
                        FloatingPosition {
                            x: FloatOrInt(
                                100.0,
                            ),
                            y: FloatOrInt(
                                -200.0,
                            ),
                            relative_to: BottomLeft,
                        },
                    ),
                    scroll_factor: None,
                    tiled_state: None,
                    background_effect: BackgroundEffectRule {
                        xray: None,
                        blur: None,
                        noise: None,
                        saturation: None,
                    },
                    popups: PopupsRule {
                        opacity: None,
                        geometry_corner_radius: None,
                        background_effect: BackgroundEffectRule {
                            xray: None,
                            blur: None,
                            noise: None,
                            saturation: None,
                        },
                    },
                },
            ],
            layer_rules: [
                LayerRule {
                    matches: [
                        Match {
                            namespace: Some(
                                RegexEq(
                                    Regex(
                                        "^notifications$",
                                    ),
                                ),
                            ),
                            at_startup: None,
                            layer: None,
                        },
                    ],
                    excludes: [],
                    opacity: None,
                    block_out_from: Some(
                        Screencast,
                    ),
                    shadow: ShadowRule {
                        off: false,
                        on: false,
                        offset: None,
                        softness: None,
                        spread: None,
                        draw_behind_window: None,
                        color: None,
                        inactive_color: None,
                    },
                    geometry_corner_radius: None,
                    place_within_backdrop: None,
                    baba_is_float: None,
                    background_effect: BackgroundEffectRule {
                        xray: None,
                        blur: None,
                        noise: None,
                        saturation: None,
                    },
                    popups: PopupsRule {
                        opacity: None,
                        geometry_corner_radius: None,
                        background_effect: BackgroundEffectRule {
                            xray: None,
                            blur: None,
                            noise: None,
                            saturation: None,
                        },
                    },
                },
            ],
            binds: Binds(
                [
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_Escape,
                            ),
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: ToggleKeyboardShortcutsInhibit,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: false,
                        hotkey_overlay_title: Some(
                            Some(
                                "Inhibit",
                            ),
                        ),
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_Escape,
                            ),
                            modifiers: Modifiers(
                                SHIFT | COMPOSITOR,
                            ),
                        },
                        action: ToggleKeyboardShortcutsInhibit,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: false,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_t,
                            ),
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: Spawn(
                            [
                                "alacritty",
                            ],
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: true,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_q,
                            ),
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: CloseWindow,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: Some(
                            None,
                        ),
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_h,
                            ),
                            modifiers: Modifiers(
                                SHIFT | COMPOSITOR,
                            ),
                        },
                        action: FocusMonitorLeft,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_o,
                            ),
                            modifiers: Modifiers(
                                SHIFT | COMPOSITOR,
                            ),
                        },
                        action: FocusMonitor(
                            "eDP-1",
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_l,
                            ),
                            modifiers: Modifiers(
                                CTRL | SHIFT | COMPOSITOR,
                            ),
                        },
                        action: MoveWindowToMonitorRight,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_o,
                            ),
                            modifiers: Modifiers(
                                CTRL | ALT | COMPOSITOR,
                            ),
                        },
                        action: MoveWindowToMonitor(
                            "eDP-1",
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_p,
                            ),
                            modifiers: Modifiers(
                                CTRL | ALT | COMPOSITOR,
                            ),
                        },
                        action: MoveColumnToMonitor(
                            "DP-1",
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_comma,
                            ),
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: ConsumeWindowIntoColumn,
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_1,
                            ),
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: FocusWorkspace(
                            Index(
                                1,
                            ),
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_1,
                            ),
                            modifiers: Modifiers(
                                SHIFT | COMPOSITOR,
                            ),
                        },
                        action: FocusWorkspace(
                            Name(
                                "workspace-1",
                            ),
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_e,
                            ),
                            modifiers: Modifiers(
                                SHIFT | COMPOSITOR,
                            ),
                        },
                        action: Quit(
                            true,
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: false,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: WheelScrollDown,
                            modifiers: Modifiers(
                                COMPOSITOR,
                            ),
                        },
                        action: FocusWorkspaceDown,
                        repeat: true,
                        cooldown: Some(
                            150ms,
                        ),
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_s,
                            ),
                            modifiers: Modifiers(
                                ALT | SUPER,
                            ),
                        },
                        action: SpawnSh(
                            "pkill orca || exec orca",
                        ),
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: true,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                ],
            ),
            switch_events: SwitchBinds {
                lid_open: None,
                lid_close: None,
                tablet_mode_on: Some(
                    SwitchAction {
                        spawn: [
                            "bash",
                            "-c",
                            "gsettings set org.gnome.desktop.a11y.applications screen-keyboard-enabled true",
                        ],
                    },
                ),
                tablet_mode_off: Some(
                    SwitchAction {
                        spawn: [
                            "bash",
                            "-c",
                            "gsettings set org.gnome.desktop.a11y.applications screen-keyboard-enabled false",
                        ],
                    },
                ),
            },
            debug: Debug {
                preview_render: None,
                dbus_interfaces_in_non_session_instances: false,
                wait_for_frame_completion_before_queueing: false,
                enable_overlay_planes: false,
                disable_cursor_plane: false,
                disable_direct_scanout: false,
                keep_max_bpc_unchanged: false,
                restrict_primary_scanout_to_matching_format: false,
                force_disable_connectors_on_resume: false,
                render_drm_device: Some(
                    "/dev/dri/renderD129",
                ),
                ignored_drm_devices: [
                    "/dev/dri/renderD128",
                    "/dev/dri/renderD130",
                ],
                force_pipewire_invalid_modifier: false,
                emulate_zero_presentation_time: false,
                disable_resize_throttling: false,
                disable_transactions: false,
                keep_laptop_panel_on_when_lid_is_closed: false,
                disable_monitor_names: false,
                strict_new_window_focus_policy: false,
                honor_xdg_activation_with_invalid_serial: false,
                deactivate_unfocused_windows: false,
                skip_cursor_only_updates_during_vrr: false,
            },
            workspaces: [
                Workspace {
                    name: WorkspaceName(
                        "workspace-1",
                    ),
                    open_on_output: Some(
                        "eDP-1",
                    ),
                    layout: None,
                },
                Workspace {
                    name: WorkspaceName(
                        "workspace-2",
                    ),
                    open_on_output: None,
                    layout: None,
                },
                Workspace {
                    name: WorkspaceName(
                        "workspace-3",
                    ),
                    open_on_output: None,
                    layout: None,
                },
            ],
            recent_windows: RecentWindows {
                on: false,
                debounce_ms: 750,
                open_delay_ms: 150,
                highlight: MruHighlight {
                    active_color: Color {
                        r: 0.0,
                        g: 1.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    urgent_color: Color {
                        r: 1.0,
                        g: 0.6,
                        b: 0.6,
                        a: 1.0,
                    },
                    padding: 15.0,
                    corner_radius: 0.0,
                },
                previews: MruPreviews {
                    max_height: 960.0,
                    max_scale: 0.5,
                },
                binds: [
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_Tab,
                            ),
                            modifiers: Modifiers(
                                ALT,
                            ),
                        },
                        action: MruAdvance {
                            direction: Forward,
                            scope: None,
                            filter: Some(
                                All,
                            ),
                        },
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_grave,
                            ),
                            modifiers: Modifiers(
                                ALT,
                            ),
                        },
                        action: MruAdvance {
                            direction: Forward,
                            scope: None,
                            filter: Some(
                                AppId,
                            ),
                        },
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                    Bind {
                        key: Key {
                            trigger: Keysym(
                                XK_Tab,
                            ),
                            modifiers: Modifiers(
                                SUPER,
                            ),
                        },
                        action: MruAdvance {
                            direction: Forward,
                            scope: Some(
                                Output,
                            ),
                            filter: Some(
                                All,
                            ),
                        },
                        repeat: true,
                        cooldown: None,
                        allow_when_locked: false,
                        allow_inhibiting: true,
                        hotkey_overlay_title: None,
                    },
                ],
            },
            session_restore: SessionRestore {
                off: true,
                state_path: None,
                launch_commands: [],
                cwd_from_child: [
                    "org.kde.konsole",
                    "org.kde.yakuake",
                    "Alacritty",
                    "kitty",
                    "foot",
                    "org.wezfurlong.wezterm",
                ],
            },
        }
        "#);
    }

    fn diff_lines(expected: &str, actual: &str) -> String {
        let mut output = String::new();
        let mut in_change = false;

        for change in diff::lines(expected, actual) {
            match change {
                diff::Result::Both(_, _) => {
                    in_change = false;
                }
                diff::Result::Left(line) => {
                    if !output.is_empty() && !in_change {
                        output.push('\n');
                    }
                    output.push('-');
                    output.push_str(line);
                    output.push('\n');
                    in_change = true;
                }
                diff::Result::Right(line) => {
                    if !output.is_empty() && !in_change {
                        output.push('\n');
                    }
                    output.push('+');
                    output.push_str(line);
                    output.push('\n');
                    in_change = true;
                }
            }
        }

        output
    }

    #[test]
    fn diff_empty_to_default() {
        // We try to write the config defaults in such a way that empty sections (and an empty
        // config) give the same outcome as the default config bundled with naru. This test
        // verifies the actual differences between the two.
        let mut default_config = Config::load_default();
        let empty_config = Config::parse_mem("").unwrap();

        // Some notable omissions: the default config has some window rules, and an empty config
        // will not have any binds. Clear them out so they don't spam the diff.
        default_config.window_rules.clear();
        default_config.binds.0.clear();

        assert_snapshot!(
            diff_lines(
                &format!("{empty_config:#?}"),
                &format!("{default_config:#?}")
            ),
            @r#"
        -            numlock: false,
        +            numlock: true,

        -            tap: false,
        +            tap: true,

        -            natural_scroll: false,
        +            natural_scroll: true,

        -    spawn_at_startup: [],
        +    spawn_at_startup: [
        +        SpawnAtStartup {
        +            command: [
        +                "waybar",
        +            ],
        +        },
        +    ],

        -                0.3333333333333333,
        +                0.33333,

        -                0.6666666666666666,
        +                0.66667,
        "#,
        );
    }
}
