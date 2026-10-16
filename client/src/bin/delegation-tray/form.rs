//! The "Add server…" form, run as `delegation-tray --add-server-form
//! [--url URL] [--note TEXT]`. It's a separate process so its egui window
//! never fights the tray's event loop. It reports back on stdout with one
//! JSON line, which the tray reads:
//!
//!   {"existing": "https://..."}            already paired with that server
//!   {"enrolled": {"url": ..., "client_id": ..., "signing_key_b64": ...}}
//!
//! and prints nothing if the user closes it.

use crate::store::{normalize_url, Store};
use delegation_client::identity::{self, Identity};
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::mpsc;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum FormResult {
    Existing(String),
    Enrolled {
        url: String,
        client_id: String,
        signing_key_b64: String,
    },
}

pub fn run(args: &[String]) {
    let mut url = String::new();
    let mut note = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--url" => url = it.next().cloned().unwrap_or_default(),
            "--note" => note = it.next().cloned(),
            _ => {}
        }
    }
    let _ = delegation_client::log::init_file(&delegation_client::paths::log_dir().join("tray.log"));

    // Coming from the self-heal path, the URL is known and needs a new code.
    let step = if url.is_empty() { Step::Url } else { Step::Code };
    let form = Form {
        url,
        code: String::new(),
        note,
        step,
        error: None,
        pending: None,
        focus: true,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Delegation: add server")
            .with_inner_size([420.0, 230.0])
            .with_resizable(false)
            .with_always_on_top()
            .with_active(true),
        centered: true,
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("delegation-add-server", options, Box::new(|_cc| Ok(Box::new(form)))) {
        log::error!(target: "tray", "add-server form failed to open: {e}");
    }
}

#[derive(PartialEq)]
enum Step {
    Url,
    Code,
}

struct Form {
    url: String,
    code: String,
    note: Option<String>,
    step: Step,
    error: Option<String>,
    /// Set while an enrollment request is running on a helper thread.
    pending: Option<mpsc::Receiver<Result<Identity, String>>>,
    focus: bool,
}

impl Form {
    fn submit_url(&mut self, ctx: &egui::Context) {
        let url = match normalize_url(&self.url) {
            Ok(url) => url,
            Err(e) => {
                self.error = Some(format!("{e}"));
                return;
            }
        };
        self.url = url.clone();
        self.error = None;
        // Purely local: no network round-trip for a server we already know.
        let known = Store::peek(&Store::default_path())
            .and_then(|store| store.find(&url).map(|i| store.servers[i].identity().is_some()))
            .unwrap_or(false);
        if known {
            finish(ctx, &FormResult::Existing(url));
        } else {
            self.step = Step::Code;
            self.focus = true;
        }
    }

    fn submit_code(&mut self, ctx: &egui::Context) {
        let code = self.code.trim().to_uppercase();
        if code.is_empty() {
            self.error = Some("Enter the pairing code from the dashboard's \"Add client\" button.".into());
            return;
        }
        self.error = None;
        let (tx, rx) = mpsc::channel();
        let (url, ctx) = (self.url.clone(), ctx.clone());
        std::thread::spawn(move || {
            let result = identity::enroll(&url, &code).map_err(|e| friendly_error(&format!("{e:#}")));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.pending = Some(rx);
    }

    fn poll_enrollment(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(Ok(identity)) => {
                self.pending = None;
                finish(
                    ctx,
                    &FormResult::Enrolled {
                        url: self.url.clone(),
                        client_id: identity.client_id.clone(),
                        signing_key_b64: identity.signing_key_b64(),
                    },
                );
            }
            Ok(Err(e)) => {
                self.pending = None;
                self.error = Some(e);
                self.focus = true;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                self.error = Some("Enrollment stopped unexpectedly.".into());
            }
        }
    }
}

impl eframe::App for Form {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_enrollment(&ctx);
        let busy = self.pending.is_some();
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));

        egui::CentralPanel::default().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            if let Some(note) = &self.note {
                ui.label(egui::RichText::new(note).strong());
            }
            match self.step {
                Step::Url => {
                    ui.label("Server address");
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.url)
                            .hint_text("https://llm.example.com")
                            .desired_width(f32::INFINITY),
                    );
                    if std::mem::take(&mut self.focus) {
                        field.request_focus();
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Next").clicked() || (enter && !self.url.trim().is_empty()) {
                            self.submit_url(&ctx);
                        }
                        if ui.button("Cancel").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
                Step::Code => {
                    ui.label(format!("Server: {}", self.url));
                    ui.label("Pairing code (from the dashboard's \"Add client\" button, valid 10 minutes)");
                    let field = ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut self.code)
                            .hint_text("ABCD1234")
                            .desired_width(f32::INFINITY),
                    );
                    if std::mem::take(&mut self.focus) {
                        field.request_focus();
                    }
                    ui.horizontal(|ui| {
                        let add = ui.add_enabled(!busy, egui::Button::new("Add"));
                        if !busy && (add.clicked() || enter) {
                            self.submit_code(&ctx);
                        }
                        if ui.add_enabled(!busy, egui::Button::new("Back")).clicked() {
                            self.step = Step::Url;
                            self.error = None;
                            self.focus = true;
                        }
                        if ui.button("Cancel").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if busy {
                            ui.spinner();
                        }
                    });
                }
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
        });
    }
}

/// Turns enrollment errors into something a person can act on.
fn friendly_error(e: &str) -> String {
    if let Some(reason) = e.strip_prefix("enrollment rejected by server: ") {
        format!("The server rejected the code: {reason}.")
    } else if e.starts_with("enrollment request failed") {
        format!("Couldn't reach the server. Check the address and your connection.\n({e})")
    } else if e.contains("parsing enroll response") {
        "That address answered, but it doesn't look like a Delegation server.".to_string()
    } else {
        e.to_string()
    }
}

fn finish(ctx: &egui::Context, result: &FormResult) {
    if let Ok(json) = serde_json::to_string(result) {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{json}");
        let _ = out.flush();
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}
