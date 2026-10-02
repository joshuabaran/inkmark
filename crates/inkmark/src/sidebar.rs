//! Sidebar width and visibility, stored next to the recent-files list in
//! `$XDG_STATE_HOME/inkmark/sidebar`.

use std::path::PathBuf;

pub const MIN_WIDTH: f32 = 160.0;
pub const MAX_WIDTH: f32 = 480.0;
const DEFAULT_WIDTH: f32 = 240.0;

pub struct Sidebar {
    store: Option<PathBuf>,
    pub visible: bool,
    pub width: f32,
}

impl Sidebar {
    pub fn load(store: Option<PathBuf>) -> Self {
        let mut visible = true;
        let mut width = DEFAULT_WIDTH;
        if let Some(path) = &store
            && let Ok(text) = std::fs::read_to_string(path)
        {
            let mut lines = text.lines();
            if let Some(flag) = lines.next() {
                visible = flag != "0";
            }
            if let Some(parsed) = lines.next().and_then(|line| line.parse::<f32>().ok()) {
                width = parsed.clamp(MIN_WIDTH, MAX_WIDTH);
            }
        }
        Self {
            store,
            visible,
            width,
        }
    }

    pub fn save(&self) {
        let Some(store) = &self.store else { return };
        if let Some(dir) = store.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let flag = if self.visible { "1" } else { "0" };
        let _ = std::fs::write(store, format!("{flag}\n{}\n", self.width));
    }
}
