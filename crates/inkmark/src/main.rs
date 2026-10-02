use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("inkmark")
            .with_app_id("inkmark")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "inkmark",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(App { path }))
        }),
    )
}

struct App {
    path: Option<PathBuf>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| match &self.path {
            Some(path) => ui.label(path.display().to_string()),
            None => ui.label("No file open"),
        });
    }
}
