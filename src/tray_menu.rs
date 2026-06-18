use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use libtrayd::{ItemId, TrayHost};

/// Run the rofi-dmenu submenu loop for `id` and activate the selected item.
/// Spawns a thread so the event loop is not blocked.
pub fn spawn_menu_thread(host: TrayHost, rt_handle: tokio::runtime::Handle, id: ItemId) {
    std::thread::spawn(move || {
        // Prevent the runtime from thinking we're in a blocking region
        let _guard = rt_handle.enter();
        if let Err(e) = run_menu_loop(&host, &rt_handle, &id) {
            eprintln!("tray menu error for {id}: {e}");
        }
    });
}

fn run_menu_loop(host: &TrayHost, rt: &tokio::runtime::Handle, id: &ItemId) -> anyhow::Result<()> {
    let mut submenu_id: Option<u32> = None;

    loop {
        let items = rt.block_on(host.get_menu(id, submenu_id))?;
        if items.is_empty() {
            return Ok(());
        }

        let visible: Vec<&libtrayd::MenuNode> = items
            .iter()
            .filter(|i| i.visible && !i.label.is_empty())
            .collect();
        if visible.is_empty() {
            return Ok(());
        }

        let selection = match show_rofi(&visible) {
            Some(s) => s,
            None => return Ok(()),
        };

        let selected = match visible.iter().find(|i| i.label == selection) {
            Some(i) => i,
            None => return Ok(()),
        };

        if selected.is_submenu {
            submenu_id = Some(selected.id as u32);
        } else {
            rt.block_on(host.activate(id, selected.id as u32))?;
            return Ok(());
        }
    }
}

/// Invoke `rofi -dmenu` and return the chosen label, or `None` on cancel.
fn show_rofi(items: &[&libtrayd::MenuNode]) -> Option<String> {
    let mut child = Command::new("rofi")
        .args([
            "-dmenu",
            "-p",
            "Tray",
            "-theme-str",
            "listview { lines: 15; }",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Write labels to stdin
    {
        let stdin = child.stdin.as_mut()?;
        for item in items {
            let label = &item.label;
            if item.enabled {
                writeln!(stdin, "{label}").ok();
            } else {
                writeln!(stdin, "({label})").ok(); // mark disabled items
            }
        }
    } // drop stdin → EOF

    let output = child.wait().ok()?;
    if !output.success() {
        return None; // rofi was cancelled
    }

    let stdout = child.stdout.take()?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).ok()?;
    let trimmed = line.trim().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
