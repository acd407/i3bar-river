//! Tray integration using rustsni.

use std::fs::File;

use pangocairo::cairo;

use crate::button_manager::ButtonManager;
use crate::event_loop::{Action, EventLoop};
use crate::pointer_btn::PointerBtn;
use crate::shared_state::SharedState;

use rustsni::{ItemId, MenuNode, TrayEvent, TrayItem};

use crate::shared_state::CachedIcon;

/// Initialize the tray host and drain initial discovery events.
///
/// The first `poll()` may buffer items in `pending_messages` during
/// synchronous `GetAll` waits (items responding to
/// `StatusNotifierHostRegistered`).  This loop re-polls until no events
/// remain, so all items are discovered before the first render.
///
/// Returns `true` if any items were discovered (caller should redraw).
pub fn init(ss: &mut SharedState, max_scale: f64, el: &mut EventLoop) -> bool {
    match rustsni::TrayHost::new() {
        Ok(host) => ss.tray = Some(host),
        Err(e) => {
            eprintln!("tray init error: {e}");
            return false;
        }
    }

    if let Some(tray) = &ss.tray {
        let tray_fd = tray.fd();
        el.register_with_fd(tray_fd, |ctx| {
            let max_scale = ctx.state.max_bar_scale();
            while poll(&mut ctx.state.shared_state, max_scale) {
                ctx.state.draw_all(ctx.conn);
            }
            Ok(Action::Keep)
        });
    }

    let mut dirty = false;
    while poll(ss, max_scale) {
        dirty = true;
    }
    dirty
}

/// Process pending tray events. Call when the tray fd is readable.
pub fn poll(ss: &mut SharedState, max_scale: f64) -> bool {
    let tray = match &mut ss.tray {
        Some(t) => t,
        None => return false,
    };
    let max_h = (slot_size(ss.config.height as f64) * max_scale).ceil() as u32;
    match tray.poll() {
        Ok(events) => {
            for ev in &events {
                match ev {
                    TrayEvent::ItemAdded(id) | TrayEvent::ItemChanged(id) => {
                        if let Some(item) = tray.items().get(id) {
                            if let Some((data, w, h)) = resolve_icon(item, max_h) {
                                ss.tray_cache
                                    .insert(item.id.0.clone(), CachedIcon { data, w, h });
                            } else {
                                ss.tray_cache.remove(&item.id.0);
                            }
                        }
                    }
                    TrayEvent::ItemRemoved(id) => {
                        ss.tray_cache.remove(&id.0);
                    }
                    TrayEvent::MenuChanged(_) | TrayEvent::MenuActivationRequested(_) => {}
                    TrayEvent::HostShutdown => {}
                }
            }
            !events.is_empty()
        }
        Err(e) => {
            eprintln!("tray poll error: {e}");
            false
        }
    }
}

/// Handle a click on the tray item identified by `id`.
pub fn handle_click(ss: &mut SharedState, id: &str, btn: PointerBtn) -> bool {
    let tray = match &mut ss.tray {
        Some(t) => t,
        None => return false,
    };
    let id = ItemId(id.to_owned());
    let item = tray.items().get(&id);
    let has_menu = item.is_some_and(|i| i.has_menu());

    match btn {
        PointerBtn::Left => {
            if item.is_some_and(|i| i.item_is_menu) && has_menu {
                show_menu(ss, &id);
            } else {
                let _ = tray.activate(&id, 0, 0);
            }
        }
        PointerBtn::Right => {
            if has_menu {
                show_menu(ss, &id);
            } else {
                let _ = tray.context_menu(&id, 0, 0);
            }
        }
        _ => return false,
    }
    true
}

/// Show the context menu for a tray item using rofi -dmenu.
/// Spawns a thread so the event loop is not blocked.
fn show_menu(ss: &mut SharedState, id: &ItemId) {
    let tray = match &mut ss.tray {
        Some(t) => t,
        None => return,
    };

    // Get the menu layout
    let nodes = match tray.get_menu(id, 0) {
        Ok(n) => n,
        Err(_) => return,
    };

    if nodes.is_empty() {
        // No DBusMenu layout — fall back to ContextMenu method
        let _ = tray.context_menu(id, 0, 0);
        return;
    }

    let item = match tray.items().get(id) {
        Some(i) => i,
        None => return,
    };
    let bus_name = item.bus_name.clone();
    let menu_path = item.menu_path.clone();
    let dmenu_cmd = ss
        .config
        .menu_command
        .clone()
        .unwrap_or_else(|| "dmenu".to_owned());

    // Spawn dmenu in a thread
    std::thread::spawn(move || {
        if let Err(e) = run_dmenu_loop(&dmenu_cmd, &bus_name, &menu_path, &nodes) {
            eprintln!("tray menu error: {e}");
        }
    });
}

/// Run the dmenu loop. Shows menus and submenus until the user cancels or selects a leaf.
fn run_dmenu_loop(
    dmenu_cmd: &str,
    bus_name: &str,
    menu_path: &str,
    nodes: &[MenuNode],
) -> anyhow::Result<()> {
    let mut current_nodes = nodes.to_vec();

    loop {
        let visible: Vec<&MenuNode> = current_nodes
            .iter()
            .filter(|n| n.visible && !n.label.is_empty())
            .collect();
        if visible.is_empty() {
            return Ok(());
        }

        let selection = match show_dmenu(dmenu_cmd, &visible) {
            Some(s) => s,
            None => return Ok(()), // user cancelled
        };

        let selected = match visible.iter().find(|n| n.label == selection) {
            Some(n) => n,
            None => return Ok(()),
        };

        if selected.is_submenu && !selected.children.is_empty() {
            current_nodes = selected.children.clone();
        } else {
            rustsni::fire_menu_click(bus_name, menu_path, selected.id)?;
            return Ok(());
        }
    }
}

/// Show dmenu with the given menu items. Returns the selected label or None.
fn show_dmenu(cmd: &str, items: &[&MenuNode]) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // Split command into program and args
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let mut builder = Command::new(parts[0]);
    for arg in &parts[1..] {
        builder.arg(arg);
    }

    let mut child = builder
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    {
        let stdin = child.stdin.as_mut()?;
        for item in items {
            let label = if item.enabled {
                &item.label
            } else {
                // Mark disabled items
                &format!("({})", item.label)
            };
            let _ = writeln!(stdin, "{label}");
        }
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let trimmed = line.trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn slot_size(full_height: f64) -> f64 {
    (full_height - 4.0).max(16.0)
}

const PAD: f64 = 4.0;

/// Render tray icons.
pub fn render(
    context: &cairo::Context,
    ss: &SharedState,
    btns: &mut ButtonManager<String>,
    start_x: f64,
    full_height: f64,
    scale_f: f64,
) {
    if ss.tray_cache.is_empty() {
        return;
    }

    btns.clear();

    let mut ordered: Vec<(&String, &CachedIcon)> = ss.tray_cache.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let slot = slot_size(full_height);
    let phys_slot = slot * scale_f;
    let phys_full_height = full_height * scale_f;
    let phys_pad = PAD * scale_f;
    let mut phys_x = start_x * scale_f;
    let mut logical_x = start_x;

    context.save().unwrap();
    context.identity_matrix();

    for (id, icon) in &ordered {
        // Compute scale factor to fit icon within the slot (preserving aspect ratio)
        let fit = ((slot * scale_f) / icon.w as f64).min((full_height * scale_f) / icon.h as f64);

        // Premultiply alpha — Cairo ARGB32 format requires premultiplied alpha
        let mut icon_data = icon.data.clone();
        for pixel in icon_data.chunks_exact_mut(4) {
            let a = pixel[3] as u32;
            pixel[0] = (pixel[0] as u32 * a / 255) as u8;
            pixel[1] = (pixel[1] as u32 * a / 255) as u8;
            pixel[2] = (pixel[2] as u32 * a / 255) as u8;
        }

        let display_w = icon.w as f64 * fit;
        let display_h = icon.h as f64 * fit;
        let draw_x = phys_x + (phys_slot - display_w) / 2.0;
        let draw_y = (phys_full_height - display_h) / 2.0;

        let stride = (icon.w * 4) as i32;
        // SAFETY: Cairo only reads the data during paint.
        let surf = unsafe {
            match cairo::ImageSurface::create_for_data_unsafe(
                icon_data.as_ptr() as *mut u8,
                cairo::Format::ARgb32,
                icon.w as i32,
                icon.h as i32,
                stride,
            ) {
                Ok(s) => s,
                Err(_) => {
                    phys_x += phys_slot + phys_pad;
                    logical_x += slot + PAD;
                    continue;
                }
            }
        };

        context.save().unwrap();
        context.rectangle(phys_x, 0.0, phys_slot, phys_full_height);
        context.clip();
        context.translate(draw_x, draw_y);
        context.scale(fit, fit);
        context.set_source_surface(&surf, 0.0, 0.0).unwrap();
        context.source().set_filter(cairo::Filter::Best);
        context.paint().unwrap();
        context.restore().unwrap();

        drop(surf);

        btns.push(logical_x, slot, (*id).clone());
        phys_x += phys_slot + phys_pad;
        logical_x += slot + PAD;
    }

    context.restore().unwrap();
}

/// Resolve a tray item's icon to a native-endian ARGB32 pixel buffer.
/// Returns the raw pixmap at its native size — all scaling is done at render time.
fn resolve_icon(item: &TrayItem, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
    // `max_h` is only used by `resolve_icon_name` for the freedesktop-icons lookup size.
    // Raw pixmaps from D-Bus are used at their native resolution.

    // When requesting attention, prefer attention icon, but fall back
    // to the regular icon if the item doesn't provide a separate one.
    if item.status == "NeedsAttention" {
        if let Some(best) = item.best_attention_icon_pixmap() {
            return Some((best.data.clone(), best.width, best.height));
        }
        if let Some(result) = resolve_icon_name(&item.attention_icon_name, item, max_h) {
            return Some(result);
        }
        // No attention icon — fall through to regular icon below
    }

    // 1. Try raw pixmaps — pick the largest available
    if let Some(best) = item.best_icon_pixmap() {
        return Some((best.data.clone(), best.width, best.height));
    }

    // 2. Try icon name — absolute path or theme lookup
    if let Some(result) = resolve_icon_name(&item.icon_name, item, max_h) {
        return Some(result);
    }

    None
}

/// Try to load an icon by name. Checks icon_theme_path first, then system theme.
fn resolve_icon_name(name: &str, item: &TrayItem, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
    if name.is_empty() {
        return None;
    }
    if name.starts_with('/') {
        return load_image_path(name);
    }
    // Check icon_theme_path directly (app-bundled icons)
    for search_path in item.icon_search_paths() {
        let candidate = format!("{search_path}/{name}.png");
        if let Some(result) = load_image_path(&candidate) {
            return Some(result);
        }
    }
    // Fall back to system icon theme — request oversized source for sharp downscale
    let request_size = (max_h * 2).clamp(32, 256) as u16;
    let path = freedesktop_icons::lookup(name)
        .with_size(request_size)
        .with_cache()
        .find()?;
    load_image_path(path.to_str()?)
}

/// Load an image file at its native size, return as Cairo ARgb32 buffer.
fn load_image_path(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let mut file = File::open(path).ok()?;
    let mut surf = cairo::ImageSurface::create_from_png(&mut file).ok()?;
    let w = surf.width() as u32;
    let h = surf.height() as u32;
    let data = surf.data().ok()?.to_vec();
    Some((data, w, h))
}
