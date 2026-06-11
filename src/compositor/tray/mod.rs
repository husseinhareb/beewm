//! beewm's settings entry as a **real system-tray icon**.
//!
//! Instead of painting a panel on its own compositor layer, beewm registers a
//! freedesktop `org.kde.StatusNotifierItem` (SNI) on the session bus — exactly
//! the protocol Spotify, Discord, Steam and nm-applet use. Any StatusNotifier
//! host (here: beebar's tray module) then shows our icon alongside the others
//! and renders our `com.canonical.dbusmenu` when clicked.
//!
//! Architecture
//! ────────────
//! A dedicated thread runs a single-threaded Tokio runtime hosting the zbus
//! connection. Two objects are served:
//!   * `/StatusNotifierItem` — the SNI item (icon + `Menu` pointer).
//!   * `/MenuBar`            — the `com.canonical.dbusmenu` the host reads.
//!
//! The menu tree is shared from the compositor as `SharedMenu`; the host fetches
//! it fresh on every open (`AboutToShow` + `GetLayout`), so the compositor only
//! has to keep `SharedMenu` current. When the user picks an item the dbusmenu
//! `Event` handler resolves it to a [`MenuAction`] and ships it back to the
//! compositor over a calloop channel, where `Beewm::apply_menu_action` runs it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use smithay::reexports::calloop::channel::Sender;
use tokio::sync::oneshot;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value};
use zbus::{ConnectionBuilder, interface, proxy};

// ── menu model (renderer-agnostic; shared with `Beewm::build_tray_menu`) ──────

/// A leaf action a menu item performs when selected, applied by the compositor
/// in `Beewm::apply_menu_action` (resolution/refresh become backend requests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    SetGap(u32),
    SetScreenTimeout(u32),
    /// Switch the anchor output to this resolution (best refresh chosen).
    SetResolution {
        width: i32,
        height: i32,
    },
    /// Switch the anchor output to this refresh rate at the current resolution.
    SetRefresh {
        hz: u32,
    },
    SignOut,
}

/// The anchor output's mode list, used to populate the Resolution/Refresh
/// submenus. Built by the compositor from what the backend reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeMenu {
    /// Unique resolutions, caller-sorted (largest first).
    pub resolutions: Vec<(i32, i32)>,
    /// Refresh rates (Hz) available at the current resolution, largest first.
    pub refresh_rates: Vec<u32>,
    pub current_res: Option<(i32, i32)>,
    pub current_hz: Option<u32>,
}

pub enum MenuItemKind {
    Submenu(Vec<MenuItem>),
    Action(MenuAction),
    Disabled,
}

pub struct MenuItem {
    pub label: String,
    pub kind: MenuItemKind,
}

/// The menu tree shared between the compositor and the D-Bus thread. The
/// compositor rebuilds it (`Beewm::refresh_tray_menu`) whenever the inputs
/// change; the D-Bus thread reads it on every `GetLayout`.
pub type SharedMenu = Arc<Mutex<Vec<MenuItem>>>;

/// Build the settings menu. When `modes` is `Some`, the Resolution/Refresh
/// submenus are populated from the live mode list (current entry marked `✓`);
/// otherwise they show a placeholder (e.g. nested winit has no modes).
pub fn build_menu_items(
    modes: Option<&ModeMenu>,
    current_gap: u32,
    screen_timeout: u32,
) -> Vec<MenuItem> {
    let mark = |selected: bool| if selected { " ✓" } else { "" };
    let gap = |n: u32| MenuItem {
        label: format!("{n} px{}", mark(current_gap == n)),
        kind: MenuItemKind::Action(MenuAction::SetGap(n)),
    };
    let timeout = |label: String, secs: u32| MenuItem {
        label: format!("{label}{}", mark(screen_timeout == secs)),
        kind: MenuItemKind::Action(MenuAction::SetScreenTimeout(secs)),
    };
    let placeholder = |_what: &'static str| {
        vec![MenuItem {
            label: "Unavailable".into(),
            kind: MenuItemKind::Disabled,
        }]
    };
    let mut gaps = vec![0, 4, 8, 16, 32];
    if !gaps.contains(&current_gap) {
        gaps.push(current_gap);
        gaps.sort_unstable();
    }
    let mut timeouts = vec![
        ("Never".to_string(), 0),
        ("1 min".to_string(), 60),
        ("5 min".to_string(), 300),
        ("10 min".to_string(), 600),
    ];
    if !timeouts.iter().any(|&(_, secs)| secs == screen_timeout) {
        timeouts.push((format_duration(screen_timeout), screen_timeout));
        timeouts.sort_unstable_by_key(|&(_, secs)| secs);
    }

    let resolution_items = match modes {
        Some(m) if !m.resolutions.is_empty() => m
            .resolutions
            .iter()
            .map(|&(w, h)| {
                let mark = if m.current_res == Some((w, h)) {
                    " ✓"
                } else {
                    ""
                };
                MenuItem {
                    label: format!("{w}x{h}{mark}"),
                    kind: MenuItemKind::Action(MenuAction::SetResolution {
                        width: w,
                        height: h,
                    }),
                }
            })
            .collect(),
        _ => placeholder("resolution"),
    };

    let refresh_items = match modes {
        Some(m) if !m.refresh_rates.is_empty() => m
            .refresh_rates
            .iter()
            .map(|&hz| {
                let mark = if m.current_hz == Some(hz) { " ✓" } else { "" };
                MenuItem {
                    label: format!("{hz} Hz{mark}"),
                    kind: MenuItemKind::Action(MenuAction::SetRefresh { hz }),
                }
            })
            .collect(),
        _ => placeholder("refresh"),
    };

    vec![
        MenuItem {
            label: "Resolution".into(),
            kind: MenuItemKind::Submenu(resolution_items),
        },
        MenuItem {
            label: "Refresh rate".into(),
            kind: MenuItemKind::Submenu(refresh_items),
        },
        MenuItem {
            label: "Gaps".into(),
            kind: MenuItemKind::Submenu(gaps.into_iter().map(gap).collect()),
        },
        MenuItem {
            label: "Screen timeout".into(),
            kind: MenuItemKind::Submenu(
                timeouts
                    .into_iter()
                    .map(|(label, secs)| timeout(label, secs))
                    .collect(),
            ),
        },
        MenuItem {
            label: "Sign out".into(),
            kind: MenuItemKind::Action(MenuAction::SignOut),
        },
    ]
}

fn format_duration(secs: u32) -> String {
    if secs == 0 {
        "Never".into()
    } else if secs.is_multiple_of(60) {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} sec")
    }
}

// ── icon ─────────────────────────────────────────────────────────────────────

const ICON_SIZE: i32 = 32;

/// Build the tray pixmap: a white "sliders" settings glyph on transparency.
///
/// SNI pixmaps are 32-bit ARGB in network (big-endian) byte order, i.e. each
/// pixel is the bytes `[A, R, G, B]`, with straight (non-premultiplied) alpha —
/// which is exactly what beebar's `decode_sni_pixmap` expects.
fn settings_pixmap() -> (i32, i32, Vec<u8>) {
    let n = ICON_SIZE;
    let mut data = vec![0u8; (n * n * 4) as usize];
    let mut put = |x: i32, y: i32| {
        if x < 0 || y < 0 || x >= n || y >= n {
            return;
        }
        let i = ((y * n + x) * 4) as usize;
        data[i] = 0xff; // A
        data[i + 1] = 0xff; // R
        data[i + 2] = 0xff; // G
        data[i + 3] = 0xff; // B
    };
    // Three horizontal slider rails, each with a square knob at a different x.
    let rails = [(8, 21), (16, 9), (24, 18)];
    for (cy, knob_cx) in rails {
        for x in 5..(n - 5) {
            for ty in (cy - 1)..=(cy + 1) {
                put(x, ty);
            }
        }
        for kx in (knob_cx - 3)..=(knob_cx + 3) {
            for ky in (cy - 4)..=(cy + 4) {
                put(kx, ky);
            }
        }
    }
    (n, n, data)
}

// ── StatusNotifierItem ─────────────────────────────────────────────────────────

struct StatusNotifierItem {
    pixmap: (i32, i32, Vec<u8>),
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItem {
    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }
    #[zbus(property)]
    fn id(&self) -> String {
        "beewm".into()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        "beewm settings".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    /// Themed-icon fallback (used by hosts that ignore `IconPixmap`).
    #[zbus(property)]
    fn icon_name(&self) -> String {
        "preferences-desktop-display".into()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        vec![self.pixmap.clone()]
    }
    /// True ⇒ a click should present the menu rather than "activate" an app.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/MenuBar").expect("static path")
    }

    async fn activate(&self, _x: i32, _y: i32) {}
    async fn secondary_activate(&self, _x: i32, _y: i32) {}
    async fn context_menu(&self, _x: i32, _y: i32) {}
}

// ── com.canonical.dbusmenu ─────────────────────────────────────────────────────

/// One dbusmenu layout node: `(id, a{sv} props, av children)`.
type MenuNodeReturn = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

struct DbusMenu {
    shared: SharedMenu,
    action_tx: Sender<MenuAction>,
    /// id → action, rebuilt on every `GetLayout` so `Event(id)` can resolve.
    actions: HashMap<i32, MenuAction>,
    revision: u32,
}

#[derive(Debug, Clone)]
struct LayoutNode {
    id: i32,
    label: String,
    enabled: bool,
    children: Vec<LayoutNode>,
}

impl LayoutNode {
    fn root(children: Vec<LayoutNode>) -> Self {
        Self {
            id: 0,
            label: String::new(),
            enabled: true,
            children,
        }
    }

    fn is_submenu(&self) -> bool {
        !self.children.is_empty()
    }

    fn find(&self, id: i32) -> Option<&Self> {
        if self.id == id {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(id))
    }

    fn to_return(&self, depth: Option<usize>, property_names: &[String]) -> MenuNodeReturn {
        let props = if self.id == 0 {
            HashMap::new()
        } else {
            menu_props(&self.label, self.is_submenu(), self.enabled, property_names)
        };
        (
            self.id,
            props,
            self.rendered_children(depth, property_names),
        )
    }

    fn to_owned_value(&self, depth: Option<usize>, property_names: &[String]) -> OwnedValue {
        menu_node(
            self.id,
            menu_props(&self.label, self.is_submenu(), self.enabled, property_names),
            self.rendered_children(depth, property_names),
        )
    }

    fn rendered_children(
        &self,
        depth: Option<usize>,
        property_names: &[String],
    ) -> Vec<OwnedValue> {
        match depth {
            Some(0) => Vec::new(),
            Some(depth) => self
                .children
                .iter()
                .map(|child| child.to_owned_value(Some(depth - 1), property_names))
                .collect(),
            None => self
                .children
                .iter()
                .map(|child| child.to_owned_value(None, property_names))
                .collect(),
        }
    }
}

/// Recursively number `items` in pre-order and record each enabled leaf action.
fn build_layout_tree(
    items: &[MenuItem],
    next_id: &mut i32,
    actions: &mut HashMap<i32, MenuAction>,
) -> Vec<LayoutNode> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let id = *next_id;
        *next_id += 1;
        let (enabled, children) = match &item.kind {
            MenuItemKind::Submenu(sub) => (true, build_layout_tree(sub, next_id, actions)),
            MenuItemKind::Action(action) => {
                actions.insert(id, action.clone());
                (true, Vec::new())
            }
            MenuItemKind::Disabled => (false, Vec::new()),
        };
        out.push(LayoutNode {
            id,
            label: item.label.clone(),
            enabled,
            children,
        });
    }
    out
}

fn layout_for_request(
    items: &[MenuItem],
    parent_id: i32,
    recursion_depth: i32,
    property_names: &[String],
) -> (MenuNodeReturn, HashMap<i32, MenuAction>) {
    let mut actions = HashMap::new();
    let mut next_id = 1;
    let root = LayoutNode::root(build_layout_tree(items, &mut next_id, &mut actions));
    let depth = if recursion_depth < 0 {
        None
    } else {
        Some(recursion_depth as usize)
    };
    let node = root
        .find(parent_id)
        .map(|node| node.to_return(depth, property_names))
        .unwrap_or_else(|| (parent_id, HashMap::new(), Vec::new()));
    (node, actions)
}

fn property_requested(property_names: &[String], name: &str) -> bool {
    property_names.is_empty() || property_names.iter().any(|property| property == name)
}

fn menu_props(
    label: &str,
    is_submenu: bool,
    enabled: bool,
    property_names: &[String],
) -> HashMap<String, OwnedValue> {
    let mut props = HashMap::new();
    if property_requested(property_names, "label") {
        props.insert(
            "label".into(),
            OwnedValue::try_from(Value::from(label.to_string())).expect("label value"),
        );
    }
    if property_requested(property_names, "enabled") {
        props.insert(
            "enabled".into(),
            OwnedValue::try_from(Value::from(enabled)).expect("enabled value"),
        );
    }
    if property_requested(property_names, "visible") {
        props.insert(
            "visible".into(),
            OwnedValue::try_from(Value::from(true)).expect("visible value"),
        );
    }
    if is_submenu && property_requested(property_names, "children-display") {
        props.insert(
            "children-display".into(),
            OwnedValue::try_from(Value::from("submenu".to_string())).expect("submenu value"),
        );
    }
    props
}

/// Build one dbusmenu node as an `OwnedValue` variant (`(ia{sv}av)`).
fn menu_node(id: i32, props: HashMap<String, OwnedValue>, children: Vec<OwnedValue>) -> OwnedValue {
    let node = StructureBuilder::new()
        .add_field(id)
        .add_field(props)
        .add_field(children)
        .build();
    OwnedValue::try_from(Value::from(node)).expect("menu node → owned value")
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbusMenu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }
    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    async fn get_layout(
        &mut self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> (u32, MenuNodeReturn) {
        let (node, actions) = {
            let items = self.shared.lock().unwrap();
            layout_for_request(&items, parent_id, recursion_depth, &property_names)
        };
        self.actions = actions;
        self.revision = self.revision.wrapping_add(1);
        (self.revision, node)
    }

    async fn event(&mut self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let Some(action) = self.actions.get(&id).cloned() else {
            return;
        };
        if let Err(error) = self.action_tx.send(action) {
            tracing::warn!(target: "beewm::tray", %error, "tray action channel closed");
        }
    }

    async fn about_to_show(&self, _id: i32) -> bool {
        // The host re-reads the layout via GetLayout right after this, so the
        // return value (does-the-layout-need-updating) doesn't matter.
        true
    }
}

// ── StatusNotifierWatcher proxy (registration target) ──────────────────────────

#[proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

// ── thread entry point ─────────────────────────────────────────────────────────

pub struct TrayHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn shutdown(mut self) {
        self.request_shutdown();
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .map(|thread| thread.is_finished())
            .unwrap_or(true)
    }

    fn request_shutdown(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

/// Spawn the tray on a dedicated thread. Returns immediately; failures are
/// logged and never crash the compositor (a missing session bus or tray host
/// just means no icon).
pub fn spawn(shared: SharedMenu, action_tx: Sender<MenuAction>) -> Option<TrayHandle> {
    let builder = std::thread::Builder::new().name("beewm-tray".into());
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    match builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(error) => {
                tracing::warn!(target: "beewm::tray", %error, "tray: tokio runtime failed");
                return;
            }
        };
        if let Err(error) = rt.block_on(run(shared, action_tx, shutdown_rx)) {
            tracing::warn!(target: "beewm::tray", %error, "tray: D-Bus loop exited");
        }
    }) {
        Ok(thread) => Some(TrayHandle {
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }),
        Err(error) => {
            tracing::warn!(target: "beewm::tray", %error, "tray: failed to spawn thread");
            None
        }
    }
}

async fn run(
    shared: SharedMenu,
    action_tx: Sender<MenuAction>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> zbus::Result<()> {
    // A pid-qualified well-known name, per the SNI convention.
    let well_known = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());

    let item = StatusNotifierItem {
        pixmap: settings_pixmap(),
    };
    let menu = DbusMenu {
        shared,
        action_tx,
        actions: HashMap::new(),
        revision: 0,
    };

    let conn = ConnectionBuilder::session()?
        .name(well_known.as_str())?
        .serve_at("/StatusNotifierItem", item)?
        .serve_at("/MenuBar", menu)?
        .build()
        .await?;

    let watcher = StatusNotifierWatcherProxy::new(&conn).await?;

    // (Re)register on a heartbeat. Re-registering an existing item is a no-op
    // for the host, so this both wins the startup race (beewm and the bar start
    // together) and recovers automatically if the bar/watcher restarts.
    let mut registered = false;
    loop {
        match watcher.register_status_notifier_item(&well_known).await {
            Ok(()) => {
                if !registered {
                    tracing::info!(target: "beewm::tray", name = %well_known, "registered tray item");
                    registered = true;
                }
            }
            Err(error) => {
                if registered {
                    tracing::warn!(target: "beewm::tray", %error, "tray watcher lost; will retry");
                }
                registered = false;
            }
        }
        let delay = if registered { 30 } else { 3 };
        if tokio::time::timeout(Duration::from_secs(delay), &mut shutdown_rx)
            .await
            .is_ok()
        {
            tracing::info!(target: "beewm::tray", "tray shutdown requested");
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::serialized::Context;
    use zbus::zvariant::{Endian, Type, to_bytes};

    /// The return value must encode to the exact dbusmenu `GetLayout` wire type
    /// `(u(ia{sv}av))`, and the value built from real menu items must actually
    /// serialize (this is what catches a malformed `StructureBuilder` node).
    #[test]
    fn layout_matches_dbusmenu_wire_format() {
        assert_eq!(
            <(u32, MenuNodeReturn) as Type>::signature().to_string(),
            "(u(ia{sv}av))"
        );

        let items = build_menu_items(None, 4, 600);
        let (node, actions) = layout_for_request(&items, 0, -1, &[]);
        // Sign out is always present; disabled placeholders are not actions.
        assert!(actions.values().any(|a| *a == MenuAction::SignOut));

        let ret: (u32, MenuNodeReturn) = (1, node);
        let ctxt = Context::new_dbus(Endian::Little, 0);
        let bytes = to_bytes(ctxt, &ret).expect("dbusmenu layout encodes");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn disabled_placeholders_are_visible_but_not_actions() {
        let items = build_menu_items(None, 4, 600);
        let mut actions = HashMap::new();
        let mut next_id = 1;
        let tree = build_layout_tree(&items, &mut next_id, &mut actions);
        let resolution = tree
            .iter()
            .find(|node| node.label == "Resolution")
            .expect("resolution submenu");
        let unavailable = resolution
            .children
            .iter()
            .find(|node| node.label == "Unavailable")
            .expect("unavailable row");

        assert!(!unavailable.enabled);
        assert!(!actions.contains_key(&unavailable.id));
    }

    #[test]
    fn current_gap_and_timeout_are_marked() {
        let items = build_menu_items(None, 8, 300);
        let gaps = submenu(&items, "Gaps");
        assert!(gaps.iter().any(|item| item.label == "8 px ✓"));
        let timeouts = submenu(&items, "Screen timeout");
        assert!(timeouts.iter().any(|item| item.label == "5 min ✓"));
    }

    #[test]
    fn layout_request_can_target_submenu_and_limit_depth() {
        let items = build_menu_items(None, 4, 600);
        let mut actions = HashMap::new();
        let mut next_id = 1;
        let tree = build_layout_tree(&items, &mut next_id, &mut actions);
        let gaps_id = tree
            .iter()
            .find(|node| node.label == "Gaps")
            .expect("gaps submenu")
            .id;

        let ((id, props, children), _) =
            layout_for_request(&items, gaps_id, 0, &["label".to_string()]);
        assert_eq!(id, gaps_id);
        assert!(props.contains_key("label"));
        assert!(!props.contains_key("enabled"));
        assert!(children.is_empty());
    }

    fn submenu<'a>(items: &'a [MenuItem], label: &str) -> &'a [MenuItem] {
        let item = items
            .iter()
            .find(|item| item.label == label)
            .unwrap_or_else(|| panic!("missing submenu {label}"));
        match &item.kind {
            MenuItemKind::Submenu(items) => items,
            _ => panic!("{label} is not a submenu"),
        }
    }

    #[test]
    fn pixmap_is_full_argb_with_white_glyph() {
        let (w, h, data) = settings_pixmap();
        assert_eq!((w, h), (ICON_SIZE, ICON_SIZE));
        assert_eq!(data.len(), (w * h * 4) as usize);
        // Some fully-opaque white pixels make up the glyph, the rest transparent.
        assert!(data.chunks_exact(4).any(|p| p == [0xff, 0xff, 0xff, 0xff]));
        assert!(data.chunks_exact(4).any(|p| p[0] == 0x00));
    }
}
