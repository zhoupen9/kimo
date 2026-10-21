//! Linux system-tray icon via the StatusNotifierItem (SNI) protocol.
//!
//! The bus itself — marshalling, framing, the connection — lives in
//! `crate::dbus`; this module is the object the panel talks to. On one
//! dedicated `std::thread` it registers a `org.kde.StatusNotifierItem` and
//! serves the properties/activations that KDE Plasma and compatible trays
//! query, plus a `com.canonical.dbusmenu` context menu (Show / Quit) at
//! `MENU_PATH`. The icon is `assets/app.png` (256 px, rasterized from
//! `assets/app.svg`), embedded at compile time, decoded with the pure-Rust
//! image crate with its colors kept as-is. It is deliberately large: hosts
//! scale the pixmap down to their panel size, and the 48 px source this
//! replaced was blurry on a HiDPI panel.
//!
//! The icon has two shapes: the mark, and the mark wearing a red badge when
//! mail has arrived that the window has not shown yet (`set_unread`). The UI
//! thread only sets a flag; the swap, and the `NewIcon` signal that is the
//! only way a host learns to read `IconPixmap` again, happen on the tray
//! thread. A missing StatusNotifierWatcher (e.g. GNOME without an extension)
//! simply disables the tray — the app keeps running.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::dbus::{
    build_body, properties_dict, Bus, Unmarshal, Value, FIELD_DESTINATION, FIELD_ERROR_NAME,
    FIELD_INTERFACE, FIELD_MEMBER, FIELD_PATH, FIELD_REPLY_SERIAL, FIELD_SIGNATURE, MSG_ERROR,
    MSG_METHOD_CALL, MSG_METHOD_RETURN, MSG_SIGNAL,
};

/// The application icon (256 px, rasterized from `assets/app.svg`), embedded
/// at compile time: the tray serves it as an `IconPixmap`, and a new-mail
/// notification (`crate::notify`) writes it out as the icon the notification
/// shows.
pub(crate) const APP_PNG: &[u8] = include_bytes!("../assets/app.png");

/// Whether the tray is registered and being served. The window's minimize
/// button consults this before hiding the window: the tray is the way back,
/// and a hide with no tray would be a window out of reach.
static AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Set when Show (or a left click on the icon) asks for the window. The main
/// loop reads it when there is no window to draw: on Wayland a hidden window
/// is gone, so the request has to recreate it rather than send it a command.
static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set when Quit is chosen. The loop reads this and exits itself; the request
/// does not go through the close command, because close-to-tray would turn
/// that into a hide.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Set while mail has arrived that the window has not shown: the icon wears
/// the unread badge (`badge_argb`). The UI thread writes this; the tray thread
/// reads it at the top of its loop, and is told to within `ICON_POLL`.
static UNREAD: AtomicBool = AtomicBool::new(false);

/// Whether a tray icon is registered and listening for Show.
pub(crate) fn available() -> bool {
    AVAILABLE.load(Ordering::Relaxed)
}

/// Take the pending Show request, if any.
pub(crate) fn take_show_request() -> bool {
    SHOW_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Take the pending Quit request, if any.
pub(crate) fn take_quit_request() -> bool {
    QUIT_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Show (`true`) or clear the unread badge on the tray icon. Nothing is sent
/// from here: the tray thread owns the connection, and both halves of the
/// change — the property and the `NewIcon` signal that makes a host read it
/// again — are its own (`serve`). All this does is set the flag that thread
/// looks at, so it is safe to call from anywhere.
pub(crate) fn set_unread(unread: bool) {
    UNREAD.store(unread, Ordering::Relaxed);
}

const SNI_INTERFACE: &str = "org.kde.StatusNotifierItem";
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Where the SNI `Menu` property points: our `com.canonical.dbusmenu` object.
const MENU_PATH: &str = "/Menu";
/// Where the item itself is. `RegisterStatusNotifierItem` is handed a bus name
/// rather than an object path, and the spec puts the item of such a service
/// here — which is where the `NewIcon` signal has to claim to come from.
const ITEM_PATH: &str = "/StatusNotifierItem";
/// Menu node ids (0 is the invisible root that the layout hangs off).
const MENU_SHOW_ID: i32 = 1;
const MENU_QUIT_ID: i32 = 2;

/// How long a quiet bus may hold the tray loop before it looks at the icon the
/// UI asks for (`UNREAD`). Short enough that the badge is on the panel about
/// when the notification beside it is, long enough that an idle tray wakes
/// twice a second and does nothing.
const ICON_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// The badge, in fractions of the icon's width: where its centre sits (both
/// axes, from the top-left corner), how wide the disc is, and the clear ring
/// that separates the disc from the mark. Fractions, not pixels, because the
/// panel scales the pixmap to whatever size it draws.
const BADGE_CENTRE: f32 = 0.72;
const BADGE_RADIUS: f32 = 0.23;
const BADGE_RING: f32 = 0.04;

/// The badge's red — `danger_hover` in the palette (`crate::theme`), the one
/// red both schemes share. The tray reads no theme (it is built once, on a
/// thread with no `egui::Context`) and an icon is drawn over the panel's own
/// background rather than the app's, so it wears a red that reads on either.
const BADGE_RED: [u8; 3] = [0xE8, 0x11, 0x23];

const INTROSPECT_XML: &str = concat!(
    "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" ",
    "\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n",
    "<node>\n",
    "  <interface name=\"org.kde.StatusNotifierItem\">\n",
    "    <property name=\"Category\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Id\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Title\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Status\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"WindowId\" type=\"i\" access=\"read\"/>\n",
    "    <property name=\"IconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"IconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"OverlayIconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"AttentionIconPixmap\" type=\"a(iiay)\" access=\"read\"/>\n",
    "    <property name=\"AttentionMovieName\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"ToolTip\" type=\"(sa(ii)ss)\" access=\"read\"/>\n",
    "    <property name=\"ItemIsMenu\" type=\"b\" access=\"read\"/>\n",
    "    <property name=\"Menu\" type=\"o\" access=\"read\"/>\n",
    "    <method name=\"Activate\"><arg type=\"i\" name=\"x\"/><arg type=\"i\" name=\"y\"/></method>\n",
    "    <method name=\"SecondaryActivate\"><arg type=\"i\" name=\"x\"/><arg type=\"i\" name=\"y\"/></method>\n",
    "    <method name=\"Scroll\"><arg type=\"i\" name=\"delta\"/><arg type=\"s\" name=\"orientation\"/></method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Properties\">\n",
    "    <method name=\"Get\"><arg type=\"s\"/><arg type=\"s\"/><arg type=\"v\" direction=\"out\"/></method>\n",
    "    <method name=\"GetAll\"><arg type=\"s\"/><arg type=\"a{sv}\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Introspectable\">\n",
    "    <method name=\"Introspect\"><arg type=\"s\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "</node>\n",
);

/// Introspection for the menu object at `MENU_PATH`.
const MENU_INTROSPECT_XML: &str = concat!(
    "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" ",
    "\"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n",
    "<node>\n",
    "  <interface name=\"com.canonical.dbusmenu\">\n",
    "    <property name=\"Version\" type=\"u\" access=\"read\"/>\n",
    "    <property name=\"TextDirection\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"Status\" type=\"s\" access=\"read\"/>\n",
    "    <property name=\"IconThemePath\" type=\"as\" access=\"read\"/>\n",
    "    <method name=\"GetLayout\">\n",
    "      <arg type=\"i\" name=\"parentId\" direction=\"in\"/>\n",
    "      <arg type=\"i\" name=\"recursionDepth\" direction=\"in\"/>\n",
    "      <arg type=\"as\" name=\"propertyNames\" direction=\"in\"/>\n",
    "      <arg type=\"u\" name=\"revision\" direction=\"out\"/>\n",
    "      <arg type=\"(ia{sv}av)\" name=\"layout\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"GetGroupProperties\">\n",
    "      <arg type=\"ai\" name=\"ids\" direction=\"in\"/>\n",
    "      <arg type=\"as\" name=\"propertyNames\" direction=\"in\"/>\n",
    "      <arg type=\"a(ia{sv})\" name=\"properties\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"GetProperty\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"s\" name=\"name\" direction=\"in\"/>\n",
    "      <arg type=\"v\" name=\"value\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"Event\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"s\" name=\"eventId\" direction=\"in\"/>\n",
    "      <arg type=\"v\" name=\"data\" direction=\"in\"/>\n",
    "      <arg type=\"u\" name=\"timestamp\" direction=\"in\"/>\n",
    "    </method>\n",
    "    <method name=\"EventGroup\">\n",
    "      <arg type=\"a(isvu)\" name=\"events\" direction=\"in\"/>\n",
    "      <arg type=\"ai\" name=\"idErrors\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"AboutToShow\">\n",
    "      <arg type=\"i\" name=\"id\" direction=\"in\"/>\n",
    "      <arg type=\"b\" name=\"needUpdate\" direction=\"out\"/>\n",
    "    </method>\n",
    "    <method name=\"AboutToShowGroup\">\n",
    "      <arg type=\"ai\" name=\"ids\" direction=\"in\"/>\n",
    "      <arg type=\"ai\" name=\"updatesNeeded\" direction=\"out\"/>\n",
    "      <arg type=\"ai\" name=\"idErrors\" direction=\"out\"/>\n",
    "    </method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Properties\">\n",
    "    <method name=\"Get\"><arg type=\"s\"/><arg type=\"s\"/><arg type=\"v\" direction=\"out\"/></method>\n",
    "    <method name=\"GetAll\"><arg type=\"s\"/><arg type=\"a{sv}\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "  <interface name=\"org.freedesktop.DBus.Introspectable\">\n",
    "    <method name=\"Introspect\"><arg type=\"s\" direction=\"out\"/></method>\n",
    "  </interface>\n",
    "</node>\n",
);
// ----- SNI service -----------------------------------------------------------

/// A decoded tray icon: its pixel size plus straight-alpha ARGB32 pixels in
/// network byte order — the payload of one SNI \`IconPixmap\` entry.
struct Icon {
    width: i32,
    height: i32,
    argb: Vec<u8>,
}

/// Decode the embedded PNG icon. The icon's own colors are kept.
fn icon_argb(png: &[u8]) -> Option<Icon> {
    let decoded = image::load_from_memory(png).ok()?;
    let rgba = decoded.to_rgba8();
    let (width, height) = (rgba.width() as i32, rgba.height() as i32);
    let mut argb = Vec::with_capacity(rgba.len());
    for px in rgba.as_raw().as_chunks::<4>().0 {
        argb.push(px[3]); // a
        argb.push(px[0]); // r
        argb.push(px[1]); // g
        argb.push(px[2]); // b
    }
    Some(Icon {
        width,
        height,
        argb,
    })
}

/// The icon with the unread badge on it: the same pixmap, plus a red disc on
/// its lower right — over the plain part of the disc, clear of the feather's
/// tip — with a ring of transparency between the two so the panel's own
/// background shows through and the disc reads as a badge laid on the mark
/// rather than as part of the drawing.
fn badge_argb(plain: &Icon) -> Icon {
    let mut icon = Icon {
        width: plain.width,
        height: plain.height,
        argb: plain.argb.clone(),
    };
    let size = plain.width as f32;
    let centre = size * BADGE_CENTRE;
    let radius = size * BADGE_RADIUS;
    let ring = radius + size * BADGE_RING;
    for y in 0..plain.height {
        for x in 0..plain.width {
            // Sampled at pixel centres, so the disc comes out symmetric.
            let dx = x as f32 + 0.5 - centre;
            let dy = y as f32 + 0.5 - centre;
            let distance = (dx * dx + dy * dy).sqrt();
            if distance > ring {
                continue;
            }
            let at = ((y as usize * plain.width as usize) + x as usize) * 4;
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                // The disc, its rim anti-aliased: the mark underneath is
                // covered rather than blended, because a badge is a thing laid
                // on the icon and not a tint of it. The pixmap is ARGB — alpha
                // first, the byte order SNI asks for — so the alpha goes at
                // `at` and the red at `at + 1`.
                icon.argb[at] = (coverage * 255.0).round() as u8;
                icon.argb[at + 1] = BADGE_RED[0];
                icon.argb[at + 2] = BADGE_RED[1];
                icon.argb[at + 3] = BADGE_RED[2];
            } else {
                // The ring: a hole in the mark, so what shows between the two
                // is the panel's own background.
                icon.argb[at..at + 4].fill(0);
            }
        }
    }
    icon
}

/// The SNI `IconPixmap` value: `a(iiay)`, one (width, height, ARGB32 data)
/// struct per icon size. Hosts cannot read a bare byte array.
fn pixmap_value(icon: &Icon) -> Value {
    Value::Array {
        signature: "(iiay)".to_string(),
        values: vec![Value::Struct(vec![
            Value::Int32(icon.width),
            Value::Int32(icon.height),
            Value::bytes(&icon.argb),
        ])],
    }
}

/// An empty `IconPixmap` value: `a(iiay)` with no entries.
fn empty_pixmaps() -> Value {
    Value::Array {
        signature: "(iiay)".to_string(),
        values: Vec::new(),
    }
}

/// One `com.canonical.dbusmenu` layout node: `(ia{sv}av)`.
fn menu_node(id: i32, props: &[(&str, Value)], children: Vec<Value>) -> Value {
    Value::Struct(vec![
        Value::Int32(id),
        properties_dict(props),
        Value::Array {
            signature: "v".to_string(),
            values: children
                .into_iter()
                .map(|child| Value::Variant(Box::new(child)))
                .collect(),
        },
    ])
}

/// The label of a menu entry; `None` for the root and unknown ids.
fn menu_label(id: i32) -> Option<&'static str> {
    match id {
        MENU_SHOW_ID => Some("Show"),
        MENU_QUIT_ID => Some("Quit"),
        _ => None,
    }
}

/// A clickable menu entry (no submenu of its own).
fn menu_entry(id: i32, label: &str) -> Value {
    menu_node(
        id,
        &[
            ("label", Value::Str(label.to_string())),
            ("enabled", Value::Bool(true)),
            ("visible", Value::Bool(true)),
        ],
        Vec::new(),
    )
}

/// The layout value of a `GetLayout` reply, for the requested parent node.
fn menu_layout_for(parent: i32) -> Value {
    match menu_label(parent) {
        Some(label) => menu_entry(parent, label),
        None if parent == 0 => menu_node(
            0,
            &[("children-display", Value::Str("submenu".to_string()))],
            vec![
                menu_entry(MENU_SHOW_ID, "Show"),
                menu_entry(MENU_QUIT_ID, "Quit"),
            ],
        ),
        None => menu_node(parent, &[], Vec::new()),
    }
}

/// Properties of the menu object itself (not of its entries).
fn menu_property(name: &str) -> Option<Value> {
    match name {
        "Version" => Some(Value::UInt32(3)),
        "TextDirection" => Some(Value::Str("ltr".to_string())),
        "Status" => Some(Value::Str("normal".to_string())),
        "IconThemePath" => Some(Value::Array {
            signature: "s".to_string(),
            values: Vec::new(),
        }),
        _ => None,
    }
}

fn all_menu_properties() -> Value {
    properties_dict(&[
        ("Version", Value::UInt32(3)),
        ("TextDirection", Value::Str("ltr".to_string())),
        ("Status", Value::Str("normal".to_string())),
        (
            "IconThemePath",
            Value::Array {
                signature: "s".to_string(),
                values: Vec::new(),
            },
        ),
    ])
}

/// A menu entry's properties, for `GetProperty`/`GetGroupProperties`.
fn menu_item_properties(id: i32) -> Value {
    match menu_label(id) {
        // No `type`: libdbusmenu-gtk only builds a widget for its default
        // item type, and anything else (e.g. "normal") is dropped as unknown.
        Some(label) => properties_dict(&[
            ("label", Value::Str(label.to_string())),
            ("enabled", Value::Bool(true)),
            ("visible", Value::Bool(true)),
        ]),
        None => properties_dict(&[("children-display", Value::Str("submenu".to_string()))]),
    }
}

/// One named property of a menu entry.
fn menu_item_property(id: i32, name: &str) -> Option<Value> {
    let Value::Array { values, .. } = menu_item_properties(id) else {
        return None;
    };
    values.into_iter().find_map(|entry| {
        let Value::Struct(fields) = entry else {
            return None;
        };
        if !matches!(fields.first(), Some(Value::Str(key)) if key == name) {
            return None;
        }
        match fields.get(1) {
            Some(Value::Variant(inner)) => Some((**inner).clone()),
            _ => None,
        }
    })
}

/// The (id, event) pairs of an `Event` or `EventGroup` call. Malformed input
/// yields no events rather than killing the tray thread.
fn menu_events(member: &str, body: &[u8]) -> Vec<(i32, String)> {
    let mut u = Unmarshal::new(body);
    let mut out = Vec::new();
    if member == "Event" {
        if let (Some(id), Some(event)) = (u.int32(), u.string()) {
            out.push((id, event));
        }
        return out;
    }
    // EventGroup: a(isvu)
    u.align(4);
    let Some(len) = u.uint32() else {
        return out;
    };
    let end = u.pos + len as usize;
    while u.pos < end {
        u.align(8);
        let (Some(id), Some(event)) = (u.int32(), u.string()) else {
            break;
        };
        out.push((id, event));
        let Some(sig) = u.signature() else { break };
        if u.skip_value(&sig).is_none() || u.uint32().is_none() {
            break;
        }
    }
    out
}

/// Bring the main window back and focus it. The window may have been hidden
/// by minimize- or close-to-tray or minimised by the window manager, so both
/// states are undone before it is raised.
fn show_window(ctx: &egui::Context) {
    SHOW_REQUESTED.store(true, Ordering::Relaxed);
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
    // egui can consider an immediate repaint already served; the loop still
    // has to hear about a show when there is no window to redraw.
    crate::wake::now();
}

/// Ask the loop to end. The request travels through the wake channel rather
/// than a viewport close: close-to-tray would otherwise hide the window rather
/// than quit, and a hidden window has no viewport to receive the command.
fn quit() {
    QUIT_REQUESTED.store(true, Ordering::Relaxed);
    crate::wake::now();
}

fn sni_property(name: &str, icon: &Icon) -> Option<Value> {
    match name {
        "Category" => Some(Value::Str("ApplicationStatus".into())),
        "Id" => Some(Value::Str("kimo".into())),
        "Title" => Some(Value::Str("Kimo".into())),
        "Status" => Some(Value::Str("Active".into())),
        "WindowId" => Some(Value::Int32(0)),
        "IconName" => Some(Value::Str(String::new())),
        "IconPixmap" => Some(pixmap_value(icon)),
        "OverlayIconName" | "AttentionIconName" | "AttentionMovieName" => {
            Some(Value::Str(String::new()))
        }
        "OverlayIconPixmap" | "AttentionIconPixmap" => Some(empty_pixmaps()),
        "ToolTip" => Some(Value::Struct(vec![
            Value::Str(String::new()),
            Value::Array {
                signature: "(ii)".to_string(),
                values: Vec::new(),
            },
            Value::Str("Kimo".into()),
            Value::Str(String::new()),
        ])),
        "ItemIsMenu" => Some(Value::Bool(false)),
        "Menu" => Some(Value::ObjectPath(MENU_PATH.to_string())),
        _ => None,
    }
}

fn all_properties(icon: &Icon) -> Value {
    let names = [
        "Category",
        "Id",
        "Title",
        "Status",
        "WindowId",
        "IconName",
        "IconPixmap",
        "OverlayIconName",
        "OverlayIconPixmap",
        "AttentionIconName",
        "AttentionIconPixmap",
        "AttentionMovieName",
        "ToolTip",
        "ItemIsMenu",
        "Menu",
    ];
    let props: Vec<(&str, Value)> = names
        .iter()
        .filter_map(|n| sni_property(n, icon).map(|v| (*n, v)))
        .collect();
    properties_dict(&props)
}

/// Tell the host that `IconPixmap` has changed. The signal is the whole of the
/// protocol here: a panel draws the pixmap it was given and only reads the
/// property again when `NewIcon` says there is a new one.
fn announce_icon(bus: &mut Bus) -> Result<(), String> {
    bus.send(MSG_SIGNAL, &new_icon_fields(), &[])
}

/// Where the `NewIcon` signal claims to come from: the item's own object, the
/// SNI interface and the member. A host filters signals on all three, so all
/// three have to be the ones it registered the item under.
fn new_icon_fields() -> [(u8, Value); 3] {
    [
        (FIELD_PATH, Value::ObjectPath(ITEM_PATH.to_string())),
        (FIELD_INTERFACE, Value::Str(SNI_INTERFACE.to_string())),
        (FIELD_MEMBER, Value::Str("NewIcon".to_string())),
    ]
}

/// Serve StatusNotifierItem and DBusMenu method calls until the bus goes away.
/// `plain` and `badge` are the two icons the host may be given; which one it
/// has is decided here, from the flag the UI thread sets (`UNREAD`).
fn serve(bus: &mut Bus, ctx: &egui::Context, plain: &Icon, badge: &Icon) -> Result<(), String> {
    let mut icon = plain;
    let mut badged = false;
    loop {
        // A quiet bus holds this loop inside the socket until `ICON_POLL` runs
        // out, which is what gives the top of the loop a turn: the flag and the
        // icon it should be showing are settled before any request is answered,
        // so a host that reads `IconPixmap` in reply to `NewIcon` gets the new
        // one.
        let wanted = UNREAD.load(Ordering::Relaxed);
        if wanted != badged {
            badged = wanted;
            icon = if badged { badge } else { plain };
            announce_icon(bus)?;
        }
        let Some(msg) = bus.next_message()? else {
            continue;
        };
        if msg.kind != MSG_METHOD_CALL {
            continue; // signals / broadcast
        }
        let interface = msg.interface.as_deref().unwrap_or("");
        let member = msg.member.as_deref().unwrap_or("");
        let on_menu = msg.path.as_deref() == Some(MENU_PATH);
        // Method calls carry their own serial; replies echo it.
        let call_serial = msg.serial;
        let sender = msg.sender.clone();
        let Some(sender) = sender else { continue };
        let invalid_args = Some("org.freedesktop.DBus.Error.InvalidArgs");

        let (reply_values, error): (Vec<Value>, Option<&str>) = match (interface, member) {
            (INTROSPECTABLE, "Introspect") | ("", "Introspect") => {
                let xml = if on_menu {
                    MENU_INTROSPECT_XML
                } else {
                    INTROSPECT_XML
                };
                (vec![Value::Str(xml.to_string())], None)
            }
            (PROPERTIES, "Get") | (PROPERTIES, "Set") => {
                let mut body = Unmarshal::new(&msg.body);
                let iface = body.string().unwrap_or_default();
                let prop = body.string().unwrap_or_default();
                let value = if iface == DBUSMENU_INTERFACE {
                    menu_property(&prop)
                } else {
                    sni_property(&prop, icon)
                };
                match value {
                    Some(v) => (vec![Value::Variant(Box::new(v))], None),
                    None => (Vec::new(), invalid_args),
                }
            }
            (PROPERTIES, "GetAll") => {
                let mut body = Unmarshal::new(&msg.body);
                let iface = body.string().unwrap_or_default();
                if iface == DBUSMENU_INTERFACE {
                    (vec![all_menu_properties()], None)
                } else {
                    (vec![all_properties(icon)], None)
                }
            }
            (SNI_INTERFACE, "Activate") | (SNI_INTERFACE, "SecondaryActivate") => {
                show_window(ctx);
                (Vec::new(), None)
            }
            (SNI_INTERFACE, "Scroll") => (Vec::new(), None),
            (DBUSMENU_INTERFACE, "GetLayout") => {
                let mut body = Unmarshal::new(&msg.body);
                let parent = body.int32().unwrap_or(0);
                (vec![Value::UInt32(1), menu_layout_for(parent)], None)
            }
            (DBUSMENU_INTERFACE, "GetGroupProperties") => {
                let mut body = Unmarshal::new(&msg.body);
                let mut ids = body.int32_array().unwrap_or_default();
                if ids.is_empty() {
                    ids = vec![0, MENU_SHOW_ID, MENU_QUIT_ID];
                }
                let values = ids
                    .into_iter()
                    .map(|id| Value::Struct(vec![Value::Int32(id), menu_item_properties(id)]))
                    .collect();
                (
                    vec![Value::Array {
                        signature: "(ia{sv})".to_string(),
                        values,
                    }],
                    None,
                )
            }
            (DBUSMENU_INTERFACE, "GetProperty") => {
                let mut body = Unmarshal::new(&msg.body);
                let id = body.int32().unwrap_or(-1);
                let name = body.string().unwrap_or_default();
                match menu_item_property(id, &name) {
                    Some(v) => (vec![Value::Variant(Box::new(v))], None),
                    None => (Vec::new(), invalid_args),
                }
            }
            (DBUSMENU_INTERFACE, "Event") | (DBUSMENU_INTERFACE, "EventGroup") => {
                for (id, event) in menu_events(member, &msg.body) {
                    if event != "clicked" {
                        continue;
                    }
                    match id {
                        MENU_SHOW_ID => show_window(ctx),
                        MENU_QUIT_ID => quit(),
                        _ => {}
                    }
                }
                if member == "EventGroup" {
                    let id_errors = Value::Array {
                        signature: "i".to_string(),
                        values: Vec::new(),
                    };
                    (vec![id_errors], None)
                } else {
                    (Vec::new(), None)
                }
            }
            (DBUSMENU_INTERFACE, "AboutToShow") => (vec![Value::Bool(false)], None),
            (DBUSMENU_INTERFACE, "AboutToShowGroup") => {
                let empty = || Value::Array {
                    signature: "i".to_string(),
                    values: Vec::new(),
                };
                (vec![empty(), empty()], None)
            }
            _ => (Vec::new(), Some("org.freedesktop.DBus.Error.UnknownMethod")),
        };

        let (signature, body) = build_body(&reply_values);
        let mut fields = vec![
            (FIELD_REPLY_SERIAL, Value::UInt32(call_serial)),
            (FIELD_DESTINATION, Value::Str(sender)),
        ];
        if let Some(error) = error {
            fields.insert(0, (FIELD_ERROR_NAME, Value::Str(error.to_string())));
        }
        if !signature.is_empty() {
            fields.push((FIELD_SIGNATURE, Value::Signature(signature)));
        }
        let kind = if error.is_some() {
            MSG_ERROR
        } else {
            MSG_METHOD_RETURN
        };
        bus.send(kind, &fields, &body)?;
    }
}

/// Spawn the tray thread. Failure is non-fatal: it is logged and the app
/// runs without a tray icon.
pub fn spawn(ctx: egui::Context) {
    let Some(plain) = icon_argb(APP_PNG) else {
        eprintln!("kimo tray: could not decode the tray icon");
        return;
    };
    let badge = badge_argb(&plain);
    let result = std::thread::Builder::new()
        .name("kimo-tray".to_string())
        .spawn(move || {
            let mut bus = match Bus::connect() {
                Ok(bus) => bus,
                Err(e) => {
                    eprintln!("kimo tray: {e}");
                    return;
                }
            };
            // The badge is a flag this thread reads while it is otherwise
            // blocked on the bus, so it is the socket's idle timeout that
            // brings the loop back round (`ICON_POLL`). `call` is unaffected:
            // only an expiry before the first byte of a message counts as the
            // moment to look at the flag.
            if let Err(e) = bus.set_read_timeout(ICON_POLL) {
                eprintln!("kimo tray: shorter read timeout: {e}");
            }
            let mut run = || -> Result<(), String> {
                let unique = bus.call(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "Hello",
                    &[],
                )?;
                if unique.kind == MSG_ERROR {
                    return Err(format!("Hello failed: {:?}", unique.error_name));
                }
                let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
                let reply = bus.call(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    &[
                        Value::Str(name.clone()),
                        Value::UInt32(4), // DoNotQueue
                    ],
                )?;
                if reply.kind == MSG_ERROR {
                    return Err(format!("RequestName failed: {:?}", reply.error_name));
                }
                let registered = bus.call(
                    "org.kde.StatusNotifierWatcher",
                    "/StatusNotifierWatcher",
                    "org.kde.StatusNotifierWatcher",
                    "RegisterStatusNotifierItem",
                    &[Value::Str(name)],
                );
                match registered {
                    Ok(r) if r.kind == MSG_ERROR => {
                        return Err(format!(
                            "no system tray (StatusNotifierWatcher): {:?}",
                            r.error_name
                        ));
                    }
                    Err(e) => {
                        return Err(format!("no system tray (StatusNotifierWatcher): {e}"));
                    }
                    Ok(_) => {}
                }
                // Registered and listening: from here until `serve` returns,
                // minimize- and close-to-tray have a way back.
                AVAILABLE.store(true, Ordering::Relaxed);
                let served = serve(&mut bus, &ctx, &plain, &badge);
                AVAILABLE.store(false, Ordering::Relaxed);
                served
            };
            if let Err(e) = run() {
                eprintln!("kimo tray: {e}");
            }
        });
    if let Err(e) = result {
        eprintln!("kimo tray: could not spawn thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the wire-level assertions need this: the tray's own code goes
    // through the re-export at the top of the file.
    use crate::dbus::type_signature;

    #[test]
    fn empty_array_properties_marshal() {
        let icon = Icon {
            width: 1,
            height: 1,
            argb: vec![0, 0, 0, 0],
        };
        let v = all_properties(&icon);
        assert_eq!(type_signature(&v), "a{sv}");
        let (sig, body) = build_body(&[v]);
        assert_eq!(sig, "a{sv}");
        assert!(!body.is_empty());
    }

    #[test]
    fn icon_pixmap_is_argb_network_order() {
        // The spec types IconPixmap as a(iiay): (width, height, ARGB32 data).
        let icon = Icon {
            width: 2,
            height: 1,
            argb: vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        };
        let v = sni_property("IconPixmap", &icon).unwrap();
        assert_eq!(type_signature(&v), "a(iiay)");
        let Value::Array { values, .. } = &v else {
            panic!("IconPixmap must be an array");
        };
        let Value::Struct(entry) = &values[0] else {
            panic!("each IconPixmap entry must be a (iiay) struct");
        };
        assert_eq!(
            entry,
            &vec![Value::Int32(2), Value::Int32(1), Value::bytes(&icon.argb),]
        );
        // Straight alpha, network byte order: alpha first, then R, G, B.
        assert_eq!(&icon.argb[..4], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn item_points_at_the_menu_object() {
        let icon = Icon {
            width: 1,
            height: 1,
            argb: vec![0; 4],
        };
        assert_eq!(
            sni_property("Menu", &icon),
            Some(Value::ObjectPath(MENU_PATH.to_string()))
        );
        assert_eq!(sni_property("ItemIsMenu", &icon), Some(Value::Bool(false)));
        assert_eq!(menu_property("Version"), Some(Value::UInt32(3)));
        assert_eq!(menu_property("Status"), Some(Value::Str("normal".into())));
    }

    /// Walk a `GetLayout` reply far enough to prove its shape: revision 1,
    /// root id 0, then the Show and Quit entries.
    #[test]
    fn menu_layout_exposes_show_and_quit() {
        let (signature, body) = build_body(&[Value::UInt32(1), menu_layout_for(0)]);
        assert_eq!(signature, "u(ia{sv}av)");

        let mut u = Unmarshal::new(&body);
        assert_eq!(u.uint32(), Some(1));
        u.align(8);
        assert_eq!(u.int32(), Some(0));
        let props_len = u.uint32().unwrap() as usize;
        u.align(8);
        let props_end = u.pos + props_len;
        assert_eq!(u.string().as_deref(), Some("children-display"));
        assert_eq!(u.signature().as_deref(), Some("s"));
        assert_eq!(u.string().as_deref(), Some("submenu"));
        assert_eq!(u.pos, props_end);

        let children_len = u.uint32().unwrap() as usize;
        let children_end = u.pos + children_len;
        let mut entries = Vec::new();
        while u.pos < children_end {
            assert_eq!(u.signature().as_deref(), Some("(ia{sv}av)"));
            u.align(8);
            let id = u.int32().unwrap();
            let props_len = u.uint32().unwrap() as usize;
            u.align(8);
            let props_end = u.pos + props_len;
            u.align(8);
            assert_eq!(u.string().as_deref(), Some("label"));
            assert_eq!(u.signature().as_deref(), Some("s"));
            entries.push((id, u.string().unwrap()));
            u.pos = props_end;
            assert_eq!(u.uint32(), Some(0)); // no submenu
        }
        assert_eq!(
            entries,
            vec![(1, "Show".to_string()), (2, "Quit".to_string())]
        );
    }

    /// libdbusmenu-gtk only builds a widget for items whose `type` it knows
    /// (or that leave it unset); `type: "normal"` silently produced an empty
    /// menu in waybar, so standard entries must not declare one.
    #[test]
    fn standard_items_do_not_declare_a_type() {
        for id in [MENU_SHOW_ID, MENU_QUIT_ID] {
            let Value::Array { values, .. } = menu_item_properties(id) else {
                panic!("properties must be a dict");
            };
            let keys: Vec<&str> = values
                .iter()
                .filter_map(|entry| match entry {
                    Value::Struct(fields) => match fields.first() {
                        Some(Value::Str(key)) => Some(key.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert!(keys.contains(&"label"), "entry {id} has no label: {keys:?}");
            assert!(
                !keys.contains(&"type"),
                "entry {id} declares a type: {keys:?}"
            );
        }
    }

    #[test]
    fn menu_events_parse_both_call_shapes() {
        let (_, event) = build_body(&[
            Value::Int32(MENU_QUIT_ID),
            Value::Str("clicked".into()),
            Value::Variant(Box::new(Value::Str(String::new()))),
            Value::UInt32(0),
        ]);
        assert_eq!(
            menu_events("Event", &event),
            vec![(MENU_QUIT_ID, "clicked".to_string())]
        );

        let (_, group) = build_body(&[Value::Array {
            signature: "(isvu)".into(),
            values: vec![
                Value::Struct(vec![
                    Value::Int32(MENU_SHOW_ID),
                    Value::Str("clicked".into()),
                    Value::Variant(Box::new(Value::Bool(true))),
                    Value::UInt32(1),
                ]),
                Value::Struct(vec![
                    Value::Int32(MENU_QUIT_ID),
                    Value::Str("hovered".into()),
                    Value::Variant(Box::new(Value::Str("x".into()))),
                    Value::UInt32(2),
                ]),
            ],
        }]);
        assert_eq!(
            menu_events("EventGroup", &group),
            vec![
                (MENU_SHOW_ID, "clicked".to_string()),
                (MENU_QUIT_ID, "hovered".to_string())
            ]
        );
    }

    #[test]
    fn embedded_png_decodes_with_its_colors() {
        let icon = icon_argb(APP_PNG).expect("decodes");
        // Large on purpose: hosts downscale it, and a small source looked
        // blurry once the panel asked for more pixels than it had.
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.argb.len(), 256 * 256 * 4);
        // The icon's background circle is #835be1 (131, 91, 225): the
        // pixmap must contain it as straight ARGB — a regression guard
        // against recoloring that once flattened the icon into a blob.
        assert!(
            icon.argb.as_chunks::<4>().0.contains(&[255, 131, 91, 225]),
            "purple background pixels must survive"
        );
        // Corners are outside the circle: fully transparent.
        assert_eq!(&icon.argb[..4], &[0, 0, 0, 0]);
    }

    /// What the badge is: the same mark, with an opaque red disc on its lower
    /// right and a clear ring between the two — and everywhere else the mark's
    /// own pixels, untouched.
    #[test]
    fn the_badge_is_a_red_disc_on_the_mark() {
        let plain = icon_argb(APP_PNG).expect("decodes");
        let badge = badge_argb(&plain);
        assert_eq!((badge.width, badge.height), (plain.width, plain.height));
        let size = plain.width;
        let pixel = |icon: &Icon, x: i32, y: i32| {
            let at = ((y as usize * size as usize) + x as usize) * 4;
            [
                icon.argb[at],
                icon.argb[at + 1],
                icon.argb[at + 2],
                icon.argb[at + 3],
            ]
        };
        let centre = (size as f32 * BADGE_CENTRE) as i32;
        // Opaque, and in the pixmap's own order: alpha first, then R, G, B.
        // Writing it R,G,B,A put 232 in the alpha slot and 255 in the blue
        // one, which the panel drew faithfully as a blue dot.
        assert_eq!(
            pixel(&badge, centre, centre),
            [255, BADGE_RED[0], BADGE_RED[1], BADGE_RED[2]]
        );
        // The ring is a hole in the mark, not a lighter red: what shows there
        // is the panel's own background.
        let ring = (size as f32 * (BADGE_CENTRE + BADGE_RADIUS + BADGE_RING / 2.0)) as i32;
        assert_eq!(pixel(&badge, ring, centre)[3], 0, "the ring must be clear");
        // The opposite corner belongs to the mark alone.
        assert_eq!(pixel(&badge, 24, 232), pixel(&plain, 24, 232));
        assert_eq!(badge.argb.len(), plain.argb.len());
    }

    /// A host matches the icon signal on sender, path and member; the path is
    /// the spec's, not one we pick (`RegisterStatusNotifierItem` was given a
    /// bus name, so the item is at the default object).
    #[test]
    fn the_icon_signal_names_the_item() {
        assert_eq!(
            new_icon_fields(),
            [
                (
                    FIELD_PATH,
                    Value::ObjectPath("/StatusNotifierItem".to_string())
                ),
                (
                    FIELD_INTERFACE,
                    Value::Str("org.kde.StatusNotifierItem".to_string())
                ),
                (FIELD_MEMBER, Value::Str("NewIcon".to_string())),
            ]
        );
    }
}
