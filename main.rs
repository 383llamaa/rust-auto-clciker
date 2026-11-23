use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Sense, Stroke};
use rdev::{listen, simulate, Button, Event, EventType, Key};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Global hotkey that turns the spammer on/off (works even when the window isn't focused).
const TOGGLE_KEY: Key = Key::F6;
/// Cancels "add to ignore list" capture mode.
const CANCEL_KEY: Key = Key::Escape;

/// How often the worker re-checks state while idle (cheap, no busy loop).
const IDLE_POLL: Duration = Duration::from_millis(20);
/// Hard caps so the echo-tracking list can never grow without bound.
const MAX_PENDING_PER_EVENT: u32 = 8;
const MAX_PENDING_ENTRIES: usize = 32;

const SPLASH_SECS: f32 = 2.6;
const FADE_SECS: f32 = 0.45;

// ───────────────────────────── input model ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Input {
    Key(Key),
    Mouse(Button),
}

impl Input {
    fn label(&self) -> String {
        match self {
            Input::Key(k) => format!("{:?}", k),
            Input::Mouse(b) => format!("Mouse {:?}", b),
        }
    }
    fn event(&self, pressed: bool) -> EventType {
        match (*self, pressed) {
            (Input::Key(k), true) => EventType::KeyPress(k),
            (Input::Key(k), false) => EventType::KeyRelease(k),
            (Input::Mouse(b), true) => EventType::ButtonPress(b),
            (Input::Mouse(b), false) => EventType::ButtonRelease(b),
        }
    }
}

struct Shared {
    running: AtomicBool,
    enabled: AtomicBool,
    cps: AtomicU32,
    window_focused: AtomicBool,
    capturing: AtomicBool,
    ignore: Mutex<Vec<Input>>,
    held: Mutex<Vec<Input>>,
    /// Events we injected ourselves (input, pressed, count) so the listener can
    /// tell them apart from real input. Tiny, capped, and entries are removed at 0.
    pending: Mutex<Vec<(Input, bool, u32)>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(true),
            enabled: AtomicBool::new(false),
            cps: AtomicU32::new(20),
            window_focused: AtomicBool::new(true),
            capturing: AtomicBool::new(false),
            ignore: Mutex::new(Vec::with_capacity(16)),
            held: Mutex::new(Vec::with_capacity(8)),
            pending: Mutex::new(Vec::with_capacity(MAX_PENDING_ENTRIES)),
        }
    }
}

/// Lock that survives a poisoned mutex instead of panicking the whole app.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ───────────────────────── echo tracking & injection ─────────────────────────

fn note_injected(s: &Shared, input: Input, pressed: bool) {
    let mut p = lock(&s.pending);
    if let Some(e) = p.iter_mut().find(|e| e.0 == input && e.1 == pressed) {
        e.2 = (e.2 + 1).min(MAX_PENDING_PER_EVENT);
        return;
    }
    if p.len() >= MAX_PENDING_ENTRIES {
        p.remove(0);
    }
    p.push((input, pressed, 1));
}

/// Returns true if this event was one we injected (and consumes it).
fn consume_echo(s: &Shared, input: Input, pressed: bool) -> bool {
    let mut p = lock(&s.pending);
    if let Some(i) = p.iter().position(|e| e.0 == input && e.1 == pressed) {
        p[i].2 -= 1;
        if p[i].2 == 0 {
            p.swap_remove(i);
        }
        true
    } else {
        false
    }
}

fn inject(s: &Shared, input: Input, pressed: bool) {
    note_injected(s, input, pressed);
    if simulate(&input.event(pressed)).is_err() {
        let _ = consume_echo(s, input, pressed);
    }
}

fn click(s: &Shared, t: Input) {
    // The physical key/button is already down, so release + press = one extra click.
    inject(s, t, false);
    inject(s, t, true);
    // If you let go in between, don't leave the input stuck down.
    if !lock(&s.held).contains(&t) {
        inject(s, t, false);
    }
}

// ───────────────────────────── listener thread ─────────────────────────────

fn on_press(s: &Shared, input: Input) {
    if input == Input::Key(TOGGLE_KEY) {
        s.enabled.fetch_xor(true, Relaxed);
        lock(&s.held).clear(); // drop any stale state on every toggle
        return;
    }
    if s.capturing.load(Relaxed) {
        s.capturing.store(false, Relaxed);
        if input != Input::Key(CANCEL_KEY) {
            let mut ig = lock(&s.ignore);
            if !ig.contains(&input) {
                ig.push(input);
            }
        }
        return;
    }
    let mut held = lock(&s.held);
    if !held.contains(&input) {
        held.push(input);
    }
}

fn on_release(s: &Shared, input: Input) {
    lock(&s.held).retain(|i| *i != input);
}

fn start_listener(s: Arc<Shared>) {
    thread::spawn(move || {
        let result = listen(move |event: Event| {
            // Mouse-move is by far the most frequent event: bail out with zero work.
            let (input, pressed) = match event.event_type {
                EventType::KeyPress(k) => (Input::Key(k), true),
                EventType::KeyRelease(k) => (Input::Key(k), false),
                EventType::ButtonPress(b) => (Input::Mouse(b), true),
                EventType::ButtonRelease(b) => (Input::Mouse(b), false),
                _ => return,
            };
            if consume_echo(&s, input, pressed) {
                return;
            }
            if pressed {
                on_press(&s, input);
            } else {
                on_release(&s, input);
            }
        });
        if let Err(e) = result {
            eprintln!("Input listener failed: {:?}", e);
        }
    });
}

// ───────────────────────────── worker thread ─────────────────────────────

fn start_worker(s: Arc<Shared>) {
    thread::spawn(move || {
        let mut targets: Vec<Input> = Vec::with_capacity(8); // reused every tick
        let mut next = Instant::now();

        while s.running.load(Relaxed) {
            targets.clear();
            if s.enabled.load(Relaxed) && !s.window_focused.load(Relaxed) {
                targets.extend(lock(&s.held).iter().copied());
                if !targets.is_empty() {
                    let ig = lock(&s.ignore);
                    targets.retain(|t| !ig.contains(t));
                }
            }

            if targets.is_empty() {
                thread::sleep(IDLE_POLL);
                next = Instant::now();
                continue;
            }

            for t in &targets {
                click(&s, *t);
            }

            // Drift-free pacing: schedule against a deadline, not "sleep(interval)".
            let cps = s.cps.load(Relaxed).max(1);
            next += Duration::from_secs_f64(1.0 / cps as f64);
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    });
}

// ───────────────────────────── loading screen ─────────────────────────────

fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn draw_splash(ctx: &egui::Context, t: f32) {
    let fade = if t > SPLASH_SECS {
        1.0 - (t - SPLASH_SECS) / FADE_SECS
    } else {
        1.0
    }
    .clamp(0.0, 1.0);

    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("splash"),
    ));
    let rect = ctx.screen_rect();
    let c = rect.center() - vec2(0.0, 40.0);

    painter.rect_filled(rect, 0.0, rgba(10, 12, 20, fade));

    // Breathing glow rings
    for k in 0..4 {
        let kf = k as f32;
        let r = 46.0 + kf * 20.0 + 5.0 * (t * 2.0 - kf * 0.8).sin();
        let a = (0.16 - kf * 0.035) * fade;
        painter.circle_stroke(c, r, Stroke::new(1.5, rgba(80, 200, 255, a)));
    }

    // Spinning comet of dots
    let head = t * 5.0;
    let n = 14;
    for i in 0..n {
        let f = i as f32 / n as f32;
        let ang = head - f * std::f32::consts::TAU * 0.75;
        let p = c + vec2(ang.cos(), ang.sin()) * 36.0;
        painter.circle_filled(p, 6.0 - f * 4.5, rgba(80, 200, 255, (1.0 - f) * fade));
    }

    // Pulsing core
    let pulse = 9.0 + 2.5 * (t * 4.0).sin();
    painter.circle_filled(c, pulse + 6.0, rgba(80, 255, 200, 0.12 * fade));
    painter.circle_filled(c, pulse, rgba(110, 255, 210, 0.95 * fade));

    // Title (fades in)
    let title_a = (t / 0.7).clamp(0.0, 1.0) * fade;
    painter.text(
        c + vec2(0.0, 92.0),
        Align2::CENTER_CENTER,
        "SPAM CLICKER",
        FontId::proportional(30.0),
        rgba(235, 245, 255, title_a),
    );

    // Progress bar + status
    let p = (t / SPLASH_SECS).clamp(0.0, 1.0);
    let eased = 1.0 - (1.0 - p).powi(3);
    let status = if p < 0.30 {
        "Hooking input devices..."
    } else if p < 0.65 {
        "Warming up click engine..."
    } else if p < 0.95 {
        "Loading ignore list..."
    } else {
        "Ready"
    };
    painter.text(
        c + vec2(0.0, 126.0),
        Align2::CENTER_CENTER,
        status,
        FontId::proportional(14.0),
        rgba(150, 170, 190, title_a),
    );

    let w = 220.0;
    let bar = egui::Rect::from_center_size(pos2(c.x, c.y + 150.0), vec2(w, 6.0));
    painter.rect_filled(bar, 3.0, rgba(40, 50, 70, fade));
    let fill = egui::Rect::from_min_size(bar.min, vec2(w * eased, 6.0));
    painter.rect_filled(fill, 3.0, rgba(80, 200, 255, fade));
}

// ───────────────────────────── main UI ─────────────────────────────

fn draw_main(ctx: &egui::Context, s: &Shared) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Spam Clicker");
        ui.label("Hold any key or mouse button (outside this window) and it repeats fast.");
        ui.add_space(8.0);

        let mut enabled = s.enabled.load(Relaxed);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
            let col = if enabled {
                Color32::from_rgb(80, 220, 120)
            } else {
                Color32::from_rgb(130, 130, 130)
            };
            ui.painter().circle_filled(r.center(), 5.0, col);
            ui.label(if enabled { "ACTIVE" } else { "OFF" });
        });
        if ui
            .checkbox(&mut enabled, "Enabled  (F6 toggles from anywhere)")
            .changed()
        {
            s.enabled.store(enabled, Relaxed);
            lock(&s.held).clear();
        }

        let mut cps = s.cps.load(Relaxed);
        if ui
            .add(egui::Slider::new(&mut cps, 1..=200).text("clicks / sec"))
            .changed()
        {
            s.cps.store(cps, Relaxed);
        }

        ui.separator();
        ui.strong("Ignore list");
        ui.label("These keys / mouse buttons are never spammed.");

        let capturing = s.capturing.load(Relaxed);
        let label = if capturing {
            "Press a key or mouse button... (Esc cancels)"
        } else {
            "Add to ignore list"
        };
        if ui.button(label).clicked() && !capturing {
            s.capturing.store(true, Relaxed);
        }

        ui.add_space(4.0);
        let mut ig = lock(&s.ignore); // no per-frame clone
        let mut remove: Option<usize> = None;
        egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
            if ig.is_empty() {
                ui.weak("(empty)");
            }
            for (idx, item) in ig.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(item.label());
                    if ui.small_button("Remove").clicked() {
                        remove = Some(idx);
                    }
                });
            }
        });
        if let Some(i) = remove {
            ig.remove(i);
        }
    });
}

// ───────────────────────────── app ─────────────────────────────

struct App {
    shared: Arc<Shared>,
    start: Instant,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = self.start.elapsed().as_secs_f32();
        self.shared
            .window_focused
            .store(ctx.input(|i| i.focused), Relaxed);

        if t >= SPLASH_SECS {
            draw_main(ctx, &self.shared);
        }

        if t < SPLASH_SECS + FADE_SECS {
            draw_splash(ctx, t);
            ctx.request_repaint(); // smooth animation only while the splash is up
        } else {
            ctx.request_repaint_after(Duration::from_millis(200)); // near-zero idle cost
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Lets the worker thread exit cleanly when the window closes.
        self.shared.running.store(false, Relaxed);
    }
}

fn main() -> eframe::Result<()> {
    let shared = Arc::new(Shared::new());
    start_listener(shared.clone());
    start_worker(shared.clone());

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([380.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Spam Clicker",
        options,
        Box::new(|_cc| {
            Ok(Box::new(App {
                shared,
                start: Instant::now(),
            }))
        }),
    )
}
