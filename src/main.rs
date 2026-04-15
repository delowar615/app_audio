use eframe::egui;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::sync::{Arc, Mutex, atomic::{AtomicU32, Ordering}};
use std::fs::{self, File};
use std::path::{Path};
use std::io::BufReader;
use serde::{Serialize, Deserialize};
use std::thread;
use std::time::{Duration, Instant};
use rand::Rng;

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    vol: f32,
    cur_idx: usize,
    music_folder: String,
}

struct AppState {
    settings: Settings,
    playing: bool,
    v_l: f32, v_r: f32,
    track_list: Vec<String>,
    elapsed_time: Duration,
    total_time: Duration,
    start_instant: Option<Instant>,
    should_skip: bool,
    reel_angle: f32,
    seek_request: Option<f32>,
}

struct AtomicLevels {
    l: AtomicU32,
    r: AtomicU32,
}

struct LevelSpy<S: Source<Item = f32>> {
    inner: S,
    levels: Arc<AtomicLevels>,
    count: usize,
    sum_sq: f32,
}

impl<S: Source<Item = f32>> Source for LevelSpy<S> {
    fn current_frame_len(&self) -> Option<usize> { self.inner.current_frame_len() }
    fn channels(&self) -> u16 { self.inner.channels() }
    fn sample_rate(&self) -> u32 { self.inner.sample_rate() }
    fn total_duration(&self) -> Option<Duration> { self.inner.total_duration() }
}

impl<S: Source<Item = f32>> Iterator for LevelSpy<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let s = self.inner.next()?;
        self.sum_sq += s * s;
        self.count += 1;
        if self.count >= 512 {
            let rms = (self.sum_sq / self.count as f32).sqrt();
            self.levels.l.store((rms * 1000.0) as u32, Ordering::Relaxed);
            self.levels.r.store((rms * 980.0) as u32, Ordering::Relaxed);
            self.count = 0; self.sum_sq = 0.0;
        }
        Some(s)
    }
}

fn main() -> eframe::Result<()> {
    let settings = load_settings();
    let track_list = get_tracks(&settings.music_folder);
    let state = Arc::new(Mutex::new(AppState {
        settings, playing: false, v_l: 0.0, v_r: 0.0,
        track_list, elapsed_time: Duration::ZERO, total_time: Duration::from_secs(1),
        start_instant: None, should_skip: false, reel_angle: 0.0, seek_request: None,
    }));
    let levels = Arc::new(AtomicLevels { l: AtomicU32::new(0), r: AtomicU32::new(0) });

    let audio_state = Arc::clone(&state);
    let audio_levels = Arc::clone(&levels);
    
    thread::spawn(move || {
        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();
        let mut last_idx = 99999;

        loop {
            let (play, idx, folder, vol, skip, seek) = {
                let s = audio_state.lock().unwrap();
                (s.playing, s.settings.cur_idx, s.settings.music_folder.clone(), s.settings.vol, s.should_skip, s.seek_request)
            };
            sink.set_volume(vol);

            if skip || seek.is_some() || (play && (idx != last_idx || sink.empty())) {
                sink.stop();
                let mut s = audio_state.lock().unwrap();
                if sink.empty() && !skip && play && seek.is_none() && !s.track_list.is_empty() {
                    s.settings.cur_idx = (s.settings.cur_idx + 1) % s.track_list.len();
                }
                if let Some(name) = s.track_list.get(s.settings.cur_idx) {
                    if let Ok(file) = File::open(Path::new(&folder).join(name)) {
                        if let Ok(source) = Decoder::new(BufReader::new(file)) {
                            s.total_time = source.total_duration().unwrap_or(Duration::from_secs(300));
                            let target = seek.unwrap_or(0.0);
                            let source = source.convert_samples::<f32>();
                            let spied = LevelSpy { inner: source, levels: Arc::clone(&audio_levels), count: 0, sum_sq: 0.0 };
                            sink.append(spied.skip_duration(Duration::from_secs_f32(target)));
                            s.elapsed_time = Duration::from_secs_f32(target);
                            s.start_instant = Some(Instant::now() - s.elapsed_time);
                            s.should_skip = false; s.seek_request = None;
                        }
                    }
                }
                last_idx = s.settings.cur_idx;
            }
            if play { sink.play(); } else { sink.pause(); }
            if let Ok(mut s) = audio_state.try_lock() {
                if play && !sink.empty() && !sink.is_paused() {
                    if let Some(st) = s.start_instant { s.elapsed_time = st.elapsed(); }
                    s.reel_angle -= 0.04;
                } else if sink.is_paused() || sink.empty() {
                    audio_levels.l.store(0, Ordering::Relaxed); audio_levels.r.store(0, Ordering::Relaxed);
                }
            }
            thread::sleep(Duration::from_millis(30));
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 750.0]),
        ..Default::default()
    };
    eframe::run_native("KOPEYSK ELITE RUST 22.2", options, Box::new(|_cc| Box::new(KopeyskApp { state, levels, cur_l: 0.0, cur_r: 0.0 })))
}

struct KopeyskApp {
    state: Arc<Mutex<AppState>>,
    levels: Arc<AtomicLevels>,
    cur_l: f32, cur_r: f32,
}

impl eframe::App for KopeyskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut s = self.state.lock().unwrap();
        let target_l = self.levels.l.load(Ordering::Relaxed) as f32 / 1000.0;
        let target_r = self.levels.r.load(Ordering::Relaxed) as f32 / 1000.0;
        
        // ФИКС БАЛЛИСТИКИ: Чуть медленнее (0.08 вместо 0.1)
        let alpha = 0.08;
        self.cur_l += alpha * (target_l - self.cur_l);
        self.cur_r += alpha * (target_r - self.cur_r);

        egui::SidePanel::right("sidebar").min_width(320.0).show(ctx, |ui| {
            ui.add_space(10.0);
            ui.heading("TRACK LIST");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                let tracks = s.track_list.clone();
                for (i, name) in tracks.iter().enumerate() {
                    if ui.selectable_label(s.settings.cur_idx == i, format!("{:02}. {}", i+1, name)).clicked() {
                        s.settings.cur_idx = i; s.playing = true; s.should_skip = true;
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width();
                    ui.add_space((w - 660.0) / 2.0); 
                    self.draw_meter_egui(ui, "L-CH", self.cur_l);
                    ui.add_space(20.0);
                    self.draw_meter_egui(ui, "R-CH", self.cur_r);
                });

                ui.add_space(40.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width();
                    ui.add_space((w - 400.0) / 2.0);
                    self.draw_reel_egui(ui, s.reel_angle);
                    ui.add_space(40.0);
                    self.draw_reel_egui(ui, s.reel_angle);
                });

                ui.add_space(50.0);
                let current_track_name = if let Some(name) = s.track_list.get(s.settings.cur_idx) {
                    Path::new(name).file_stem().unwrap().to_string_lossy().to_string()
                } else { "NO TRACK".into() };
                
                ui.label(egui::RichText::new(current_track_name.to_uppercase())
                    .color(egui::Color32::from_rgb(255, 110, 0))
                    .font(egui::FontId::proportional(20.0)));
                
                ui.add_space(15.0);

                let elapsed = s.elapsed_time.as_secs_f32();
                let total = s.total_time.as_secs_f32().max(1.0);
                ui.add(egui::ProgressBar::new(elapsed / total)
                    .desired_width(700.0)
                    .text(format!("{:.1}s / {:.1}s", elapsed, total)));
                
                ui.add_space(40.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width();
                    ui.add_space((w - 450.0) / 2.0);
                    if ui.add(egui::Button::new("PREV").min_size(egui::vec2(65.0, 35.0))).clicked() && s.settings.cur_idx > 0 { s.settings.cur_idx -= 1; s.should_skip = true; }
                    if ui.add(egui::Button::new("REW").min_size(egui::vec2(65.0, 35.0))).clicked() { s.seek_request = Some((elapsed - 10.0).max(0.0)); }
                    if ui.add(egui::Button::new(if s.playing { "PAUSE" } else { "PLAY" }).min_size(egui::vec2(65.0, 35.0))).clicked() { s.playing = !s.playing; }
                    if ui.add(egui::Button::new("STOP").min_size(egui::vec2(65.0, 35.0))).clicked() { s.playing = false; s.should_skip = true; s.elapsed_time = Duration::ZERO; }
                    if ui.add(egui::Button::new("FF").min_size(egui::vec2(65.0, 35.0))).clicked() { s.seek_request = Some(elapsed + 10.0); }
                    if ui.add(egui::Button::new("NEXT").min_size(egui::vec2(65.0, 35.0))).clicked() && !s.track_list.is_empty() {
                        s.settings.cur_idx = (s.settings.cur_idx + 1) % s.track_list.len(); s.should_skip = true;
                    }
                });

                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    let w = ui.available_width();
                    ui.add_space((w - 200.0) / 2.0);
                    ui.label("VOL");
                    if ui.add(egui::Slider::new(&mut s.settings.vol, 0.0..=1.0).show_value(false)).changed() {
                        save_settings(&s.settings);
                    }
                });
            });
        });
        ctx.request_repaint();
    }
}

impl KopeyskApp {
    fn draw_meter_egui(&self, ui: &mut egui::Ui, label: &str, val: f32) {
        let (rect, _) = ui.allocate_at_least(egui::vec2(320.0, 190.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 5.0, egui::Color32::from_rgb(30, 30, 35));
        painter.rect_filled(rect.shrink(5.0), 2.0, egui::Color32::from_rgb(255, 110, 0));
        let center = egui::pos2(rect.center().x, rect.bottom() + 15.0);
        let radius = 160.0;
        let mut pts = Vec::new();
        for i in 0..51 {
            let a_rad = f32::to_radians(180.0 + 40.0 + (i as f32 * 2.0));
            pts.push(egui::pos2(center.x + a_rad.cos() * radius, center.y + a_rad.sin() * radius));
        }
        painter.add(egui::Shape::line(pts, egui::Stroke::new(2.5, egui::Color32::BLACK)));
        for i in 0..11 {
            let a_rad = f32::to_radians(180.0 + 40.0 + (i as f32 * 10.0));
            painter.line_segment([
                egui::pos2(center.x + a_rad.cos() * radius, center.y + a_rad.sin() * radius),
                egui::pos2(center.x + a_rad.cos() * (radius + 15.0), center.y + a_rad.sin() * (radius + 15.0)),
            ], egui::Stroke::new(2.0, egui::Color32::BLACK));
        }
        // ФИКС ЧУВСТВИТЕЛЬНОСТИ: Коэффициент 2.2 вместо 4.0
        let needle_a = f32::to_radians(180.0 + 40.0 + ((val * 2.2).min(1.3)/1.3 * 100.0));
        painter.line_segment([center, egui::pos2(center.x + needle_a.cos()*(radius+15.0), center.y + needle_a.sin()*(radius+15.0))],
            egui::Stroke::new(4.5, egui::Color32::from_rgb(180, 0, 0)));
        painter.text(rect.left_top() + egui::vec2(15.0, 15.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(18.0), egui::Color32::BLACK);
    }
    fn draw_reel_egui(&self, ui: &mut egui::Ui, angle: f32) {
        let (rect, _) = ui.allocate_at_least(egui::vec2(180.0, 180.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        let center = rect.center();
        let r = 85.0;
        painter.circle_stroke(center, r, egui::Stroke::new(3.0, egui::Color32::from_rgb(255, 110, 0)));
        for i in 0..3 {
            let a_rad = angle + i as f32 * (2.0 * std::f32::consts::PI / 3.0);
            let end = egui::pos2(center.x + a_rad.cos()*(r-10.0), center.y + a_rad.sin()*(r-10.0));
            painter.line_segment([center, end], egui::Stroke::new(6.0, egui::Color32::from_rgb(255, 110, 0)));
        }
    }
}

fn get_tracks(folder: &str) -> Vec<String> {
    let mut tracks = Vec::new();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension() {
                    let ext_s = ext.to_string_lossy().to_lowercase();
                    if ["mp3", "wav", "flac"].contains(&ext_s.as_str()) {
                        tracks.push(p.file_name().unwrap().to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    tracks.sort(); tracks
}
fn load_settings() -> Settings {
    let path = "settings.json";
    if let Ok(file) = File::open(path) { if let Ok(s) = serde_json::from_reader(file) { return s; } }
    Settings { vol: 0.5, cur_idx: 0, music_folder: "C:/Users/delowar/Music".to_string() }
}
fn save_settings(s: &Settings) { if let Ok(file) = File::create("settings.json") { let _ = serde_json::to_writer_pretty(file, s); } }
