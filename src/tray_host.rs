//! Tray host integration.
//!
//! Runs a libtrayd `TrayHost` in a background tokio runtime and bridges
//! `HostEvent`s into the poll-based event loop.

use crate::event_loop::{self, EventLoop};
use crate::shared_state::TrayAction;
use crate::state::State;
use libtrayd::{HostEvent, TrayHost};
use tokio::sync::broadcast;
use wayrs_client::IoMode;

/// Try to start the tray host.  Returns `true` on success, `false` if another
/// daemon is already running (caller should exit).
pub fn init(state: &mut State, el: &mut EventLoop) -> bool {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(_) => return true, // non‑fatal; run without tray
    };

    let host = match rt.block_on(TrayHost::start()) {
        Ok(h) => h,
        Err(_) => {
            eprintln!("Another StatusNotifier watcher is already running — exiting");
            return false;
        }
    };

    let handle = rt.handle().clone();
    let mut tray_rx = host.subscribe();
    state.shared_state.tray_host = Some(host);
    state.shared_state.tray_runtime = Some(rt);
    state.shared_state.tray_handle = Some(handle);

    el.add_on_idle(move |ctx| {
        // Process incoming tray events (add/update/remove)
        loop {
            match tray_rx.try_recv() {
                Ok(event) => match event {
                    HostEvent::ItemAdded(item) | HostEvent::ItemChanged(item) => {
                        // Discard stale entries with the same title or icon name
                        // but a different D‑Bus unique name (corpse from a prior
                        // session that the watcher failed to clean up).
                        ctx.state.shared_state.tray_items.retain(|id, old| {
                            *id == item.id
                                || (old.title != item.title && old.icon.name != item.icon.name)
                        });
                        ctx.state
                            .shared_state
                            .tray_items
                            .insert(item.id.clone(), item);
                    }
                    HostEvent::ItemRemoved(id) => {
                        ctx.state.shared_state.tray_items.remove(&id);
                    }
                },
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        ctx.state.draw_all(ctx.conn);

        // Process pending tray actions (clicks)
        if let Some((id, action)) = ctx.state.shared_state.pending_tray_action.take() {
            let host = ctx.state.shared_state.tray_host.as_ref().unwrap().clone();
            let handle = ctx.state.shared_state.tray_handle.as_ref().unwrap().clone();

            match action {
                TrayAction::Activate => {
                    handle.spawn(async move {
                        host.activate(&id, 0).await.ok();
                    });
                }
                TrayAction::ShowMenu => {
                    crate::tray_menu::spawn_menu_thread(host, handle, id);
                }
            }
        }

        ctx.conn.flush(IoMode::Blocking)?;
        Ok(event_loop::Action::Keep)
    });

    true
}
