//! Platform edit/window menus, with shortcuts routed into the same client
//! actions as the command palette. No second application state lives here.

use tauri::menu::{Menu, MenuItem, MenuItemKind, Submenu};
use tauri::{Emitter, Runtime};

pub fn create<R: Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let menu = Menu::default(app)?;
    let new_chat = MenuItem::with_id(app, "new-session", "New Chat", true, Some("CmdOrCtrl+N"))?;
    let file = menu.items()?.into_iter().find_map(|item| match item {
        MenuItemKind::Submenu(submenu) if submenu.text().ok().as_deref() == Some("File") => {
            Some(submenu)
        }
        _ => None,
    });
    if let Some(file) = file {
        file.insert(&new_chat, 0)?;
    } else {
        let file = Submenu::with_items(app, "File", true, &[&new_chat])?;
        menu.insert(&file, 0)?;
    }
    let workspace = Submenu::with_items(
        app,
        "Workspace",
        true,
        &[
            &MenuItem::with_id(
                app,
                "choose-repository",
                "Open Repository…",
                true,
                Some("CmdOrCtrl+O"),
            )?,
            &MenuItem::with_id(
                app,
                "focus-composer",
                "Focus Composer",
                true,
                Some("CmdOrCtrl+L"),
            )?,
            &MenuItem::with_id(
                app,
                "command-palette",
                "Command Palette…",
                true,
                Some("CmdOrCtrl+K"),
            )?,
            &MenuItem::with_id(
                app,
                "toggle-sidebar",
                "Toggle Sidebar",
                true,
                Some("CmdOrCtrl+B"),
            )?,
            &MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?,
        ],
    )?;
    menu.insert(&workspace, if cfg!(target_os = "macos") { 2 } else { 0 })?;
    Ok(menu)
}

pub fn dispatch<R: Runtime>(app: &tauri::AppHandle<R>, event: tauri::menu::MenuEvent) {
    let action = event.id.as_ref();
    if matches!(
        action,
        "new-session"
            | "choose-repository"
            | "focus-composer"
            | "command-palette"
            | "toggle-sidebar"
            | "settings"
    ) {
        let _ = app.emit("desktop-action", action);
    }
}
