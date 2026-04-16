use eframe::egui;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::sync::{Arc, Mutex, atomic::{AtomicU32, Ordering}};
use std::fs::{self, File};
use std::path::{Path};
use std::io::BufReader;
use serde::{Serialize, Deserialize};
use std::thread;
use std::time::{Duration, Instant};
use rustfft::{FftPlanner, num_complex::Complex};

const EQ_FREQS: [f32; 10] = [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];

#[derive(Clone, Default)]
struct Biquad {
    a1: f32, a2: f32, b0: f32, b1: f32, b2: f32,
    x1: f32, x2: f32, y1: f32, y2: f32,
}

impl Biquad {
    fn peaking(freq: f32, gain_db: f32, sample_rate: f32) -> Self {
        let q = 1.2; 
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let alpha = w0.sin() / (2.0 * q);
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * w0.cos();
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * w0.cos();
        let a2 = 1.0 - alpha / a;
        Self { b0: b0/a0, b1: b1/a0, b2: b2/a0, a1: a1/a0, a2: a2/a0, ..Default::default() }
    }
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1; self.x1 = x; self.y2 = self.y1; self.y1 = y;
        y
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    vol: f32, cur_idx: usize, music_folder: String, eq_gains: [f32; 10],
}

struct AppState {
    settings: Settings, playing: bool, track_list: Vec<String>,
    elapsed_time: Duration, total_time: Duration, start_instant: Option<Instant>,
    should_skip: bool, reel_angle: f32, seek_request: Option<f32>,
}

struct AtomicShared {
    levels: [AtomicU32; 2],
    spectrum: Vec<AtomicU32>,
    eq_live: Vec<AtomicU32>,
}

struct DspEngine<S: Source<Item = f32>> {
    inner: S,
    shared: Arc<AtomicShared>,
    fft_buffer: Vec<f32>,
    planner: FftPlanner<f32>,
    filters: Vec<Biquad>,
    sample_rate: f32,
    last_eq: [f32; 10],
}

impl<S: Source<Item = f32>> Source for DspEngine<S> {
    fn current_frame_len(&self) -> Option<usize> { self.inner.current_frame_len() }
    fn channels(&self) -> u16 { self.inner.channels() }
    fn sample_rate(&self) -> u32 { self.inner.sample_rate() }
    fn total_duration(&self) -> Option<Duration> { self.inner.total_duration() }
}

impl<S: Source<Item = f32>> Iterator for DspEngine<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let mut s = self.inner.next()?;
        let mut changed = false;
        for i in 0..10 {
            let val = (self.shared.eq_live[i].load(Ordering::Relaxed) as i32 - 20) as f32;
            if (val - self.last_eq[i]).abs() > 0.1 { self.last_eq[i] = val; changed = true; }
        }
        if changed { self.filters = self.last_eq.iter().enumerate().map(|(i, &g)| Biquad::peaking(EQ_FREQS[i], g, self.sample_rate)).collect(); }
        for f in self.filters.iter_mut() { s = f.process(s); }
        s = (s * 1.02).tanh(); 
        self.fft_buffer.push(s);
        if self.fft_buffer.len() >= 1024 {
            let sum_sq: f32 = self.fft_buffer.iter().take(512).map(|&x| x * x).sum();
            let rms = (sum_sq / 512.0).sqrt();
            self.shared.levels[0].store((rms * 1000.0) as u32, Ordering::Relaxed);
            self.shared.levels[1].store((rms * 950.0) as u32, Ordering::Relaxed);
            let fft = self.planner.plan_fft_forward(1024);
            let mut buffer: Vec<Complex<f32>> = self.fft_buffer.iter().map(|&x| Complex::new(x, 0.0)).collect();
            fft.process(&mut buffer);
            for i in 0..16 {
                let bin = ((i as f32 / 16.0).powf(2.2) * 450.0) as usize + 1;
                self.shared.spectrum[i].store(((buffer[bin.min(511)].norm() * 35.0).sqrt() * 10.0) as u32, Ordering::Relaxed);
            }
            self.fft_buffer.clear();
        }
        Some(s)
    }
}

fn main() -> eframe::Result<()> {
    let settings = load_settings();
    let track_list = get_tracks(&settings.music_folder);
    let state = Arc::new(Mutex::new(AppState {
        settings: settings.clone(), playing: false, track_list,
        elapsed_time: Duration::ZERO, total_time: Duration::from_secs(1),
        start_instant: None, should_skip: false, reel_angle: 0.0, seek_request: None,
    }));
    let shared = Arc::new(AtomicShared {
        levels: [AtomicU32::new(0), AtomicU32::new(0)],
        spectrum: (0..16).map(|_| AtomicU32::new(0)).collect(),
        eq_live: settings.eq_gains.iter().map(|&g| AtomicU32::new((g + 20.0) as u32)).collect(),
    });

    let (audio_state, audio_shared) = (Arc::clone(&state), Arc::clone(&shared));
    thread::spawn(move || {
        let (_stream, handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&handle).unwrap();
        let mut last_idx = 99999;
        loop {
            let cmd = { let s = audio_state.lock().unwrap(); (s.playing, s.settings.cur_idx, s.settings.music_folder.clone(), s.settings.vol, s.should_skip, s.seek_request, s.settings.eq_gains) };
            sink.set_volume(cmd.3);
            if cmd.4 || cmd.5.is_some() || (cmd.0 && (cmd.1 != last_idx || sink.empty())) {
                sink.stop();
                let mut s = audio_state.lock().unwrap();
                if sink.empty() && !cmd.4 && cmd.0 && !s.track_list.is_empty() { s.settings.cur_idx = (s.settings.cur_idx + 1) % s.track_list.len(); }
                if let Some(name) = s.track_list.get(s.settings.cur_idx) {
                    if let Ok(file) = File::open(Path::new(&cmd.2).join(name)) {
                        if let Ok(source) = Decoder::new(BufReader::new(file)) {
                            s.total_time = source.total_duration().unwrap_or(Duration::from_secs(300));
                            let target = cmd.5.unwrap_or(0.0);
                            let sr = source.sample_rate() as f32;
                            let dsp = DspEngine {
                                inner: source.convert_samples(), shared: Arc::clone(&audio_shared),
                                fft_buffer: Vec::with_capacity(1024), planner: FftPlanner::new(),
                                filters: cmd.6.iter().enumerate().map(|(i, &g)| Biquad::peaking(EQ_FREQS[i], g, sr)).collect(),
                                sample_rate: sr, last_eq: cmd.6,
                            };
                            sink.append(dsp.skip_duration(Duration::from_secs_f32(target)));
                            s.elapsed_time = Duration::from_secs_f32(target);
                            s.start_instant = Some(Instant::now() - s.elapsed_time);
                            s.should_skip = false; s.seek_request = None;
                        }
                    }
                }
                last_idx = s.settings.cur_idx;
            }
            if cmd.0 { sink.play(); } else { sink.pause(); }
            if let Ok(mut s) = audio_state.try_lock() {
                if cmd.0 && !sink.empty() && !sink.is_paused() { s.elapsed_time = s.start_instant.unwrap().elapsed(); s.reel_angle -= 0.04; }
                else if sink.is_paused() || sink.empty() { for i in 0..16 { audio_shared.spectrum[i].store(0, Ordering::Relaxed); } }
            }
            thread::sleep(Duration::from_millis(30));
        }
    });

    eframe::run_native("ELITE RUST 25.1", eframe::NativeOptions::default(), Box::new(|_cc| Box::new(KopeyskApp { state, shared, cur_l: 0.0, cur_r: 0.0, bars: vec![0.0; 16] })))
}

struct KopeyskApp { state: Arc<Mutex<AppState>>, shared: Arc<AtomicShared>, cur_l: f32, cur_r: f32, bars: Vec<f32> }

impl eframe::App for KopeyskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut s = self.state.lock().unwrap();
        let tl = self.shared.levels[0].load(Ordering::Relaxed) as f32 / 1000.0;
        let tr = self.shared.levels[1].load(Ordering::Relaxed) as f32 / 1000.0;
        self.cur_l += 0.08 * (tl - self.cur_l); self.cur_r += 0.08 * (tr - self.cur_r);
        for i in 0..16 {
            let b = self.shared.spectrum[i].load(Ordering::Relaxed) as f32 / 100.0;
            if b > self.bars[i] { self.bars[i] += 0.3 * (b - self.bars[i]); } else { self.bars[i] *= 0.85; }
        }

        ctx.set_visuals(egui::Visuals::dark());

        egui::SidePanel::right("sidebar").min_width(320.0).show(ctx, |ui| {
            ui.add_space(10.0); ui.heading("TRACK LIST"); ui.separator();
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
            let tw = ui.available_width();
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.horizontal(|ui| { ui.add_space((tw - 660.0) / 2.0); draw_meter_ui(ui, "L-CH", self.cur_l); ui.add_space(20.0); draw_meter_ui(ui, "R-CH", self.cur_r); });
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0; ui.add_space((tw - 542.0) / 2.0);
                    draw_reel_ui(ui, s.reel_angle); ui.add_space(50.0);
                    for &h in &self.bars { let r = ui.allocate_at_least(egui::vec2(7.0, 80.0), egui::Sense::hover()).0; ui.painter().rect_filled(r.with_min_y(r.bottom() - (h * 65.0).min(80.0)), 1.0, egui::Color32::from_rgb(255, 110, 0)); ui.add_space(2.0); }
                    ui.add_space(48.0); draw_reel_ui(ui, s.reel_angle);
                });
                ui.add_space(30.0);
                let name = s.track_list.get(s.settings.cur_idx).map(|n| n.to_uppercase()).unwrap_or_else(|| "NO TRACK".into());
                ui.label(egui::RichText::new(name).color(egui::Color32::from_rgb(255, 110, 0)).font(egui::FontId::proportional(18.0)));
                ui.add_space(10.0);
                let el = s.elapsed_time.as_secs_f32();
                let tot = s.total_time.as_secs_f32().max(1.0);
                ui.add(egui::ProgressBar::new(el / tot).desired_width(700.0).text(format!("{:.1}s / {:.1}s", el, tot)));
                
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    ui.add_space((tw - 460.0) / 2.0); // Центровка EQ
                    for i in 0..10 {
                        ui.vertical(|ui| {
                            ui.set_min_width(36.0);
                            let label = if EQ_FREQS[i] >= 1000.0 { format!("{}k", EQ_FREQS[i]/1000.0) } else { format!("{}", EQ_FREQS[i]) };
                            ui.label(egui::RichText::new(label).size(9.0).color(egui::Color32::GRAY));
                            let slider = ui.add(egui::Slider::new(&mut s.settings.eq_gains[i], -15.0..=15.0).vertical().show_value(false));
                            if slider.changed() { self.shared.eq_live[i].store((s.settings.eq_gains[i] + 20.0) as u32, Ordering::Relaxed); }
                            if slider.drag_released() { save_settings(&s.settings); }
                        });
                        ui.add_space(4.0);
                    }
                });
                ui.add_space(30.0);
                ui.horizontal(|ui| {
                    ui.add_space((tw - 470.0) / 2.0);
                    let bs = egui::vec2(70.0, 35.0);
                    if ui.add(egui::Button::new("PREV").min_size(bs)).clicked() && s.settings.cur_idx > 0 { s.settings.cur_idx -= 1; s.should_skip = true; }
                    ui.add_space(10.0);
                    if ui.add(egui::Button::new("REW").min_size(bs)).clicked() { s.seek_request = Some((el - 10.0).max(0.0)); }
                    ui.add_space(10.0);
                    if ui.add(egui::Button::new(if s.playing { "PAUSE" } else { "PLAY" }).min_size(bs)).clicked() { s.playing = !s.playing; }
                    ui.add_space(10.0);
                    if ui.add(egui::Button::new("STOP").min_size(bs)).clicked() { s.playing = false; s.should_skip = true; s.elapsed_time = Duration::ZERO; }
                    ui.add_space(10.0);
                    if ui.add(egui::Button::new("FF").min_size(bs)).clicked() { s.seek_request = Some(el + 10.0); }
                    ui.add_space(10.0);
                    if ui.add(egui::Button::new("NEXT").min_size(bs)).clicked() && !s.track_list.is_empty() { s.settings.cur_idx = (s.settings.cur_idx + 1) % s.track_list.len(); s.should_skip = true; }
                });
                ui.add_space(20.0);
                ui.horizontal(|ui| { ui.add_space((tw - 220.0) / 2.0); ui.label("VOL"); if ui.add(egui::Slider::new(&mut s.settings.vol, 0.0..=1.0).show_value(false)).changed() { save_settings(&s.settings); } });
            });
        });
        ctx.request_repaint();
    }
}

fn draw_meter_ui(ui: &mut egui::Ui, label: &str, val: f32) {
    let (rect, _) = ui.allocate_at_least(egui::vec2(320.0, 190.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 5.0, egui::Color32::from_rgb(30, 30, 35));
    painter.rect_filled(rect.shrink(5.0), 2.0, egui::Color32::from_rgb(255, 110, 0));
    let center = egui::pos2(rect.center().x, rect.bottom() + 15.0);
    let radius = 160.0;
    
    let mut arc_pts = Vec::new();
    for i in 0..=50 {
        let a = f32::to_radians(180.0 + 40.0 + (i as f32 * 2.0));
        arc_pts.push(egui::pos2(center.x + a.cos()*radius, center.y + a.sin()*radius));
    }
    painter.add(egui::Shape::line(arc_pts, egui::Stroke::new(2.0, egui::Color32::BLACK)));

    for i in 0..11 {
        let a = f32::to_radians(180.0 + 40.0 + (i as f32 * 10.0));
        painter.line_segment([egui::pos2(center.x + a.cos()*radius, center.y + a.sin()*radius), 
            egui::pos2(center.x + a.cos()*(radius+12.0), center.y + a.sin()*(radius+12.0))], egui::Stroke::new(2.0, egui::Color32::BLACK));
    }

    // ФИКС replace_alpha -> Color32::from_rgba_unmultiplied
    painter.text(center + egui::vec2(0.0, -radius*0.4), egui::Align2::CENTER_CENTER, "VU", 
        egui::FontId::proportional(22.0), egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140));

    let a_n = f32::to_radians(180.0 + 40.0 + ((val * 1.8).min(1.3)/1.3 * 100.0));
    painter.line_segment([center, egui::pos2(center.x + a_n.cos()*(radius+15.0), center.y + a_n.sin()*(radius+15.0))],
        egui::Stroke::new(4.5, egui::Color32::from_rgb(180, 0, 0)));
    painter.text(rect.left_top()+egui::vec2(15.0,15.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(18.0), egui::Color32::BLACK);
}

fn draw_reel_ui(ui: &mut egui::Ui, angle: f32) {
    let (rect, _) = ui.allocate_at_least(egui::vec2(150.0, 150.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let c = rect.center();
    painter.circle_stroke(c, 70.0, egui::Stroke::new(3.0, egui::Color32::from_rgb(255, 110, 0)));
    for i in 0..3 { let a = angle + i as f32 * 2.09; painter.line_segment([c, egui::pos2(c.x + a.cos()*60.0, c.y + a.sin()*60.0)], egui::Stroke::new(5.0, egui::Color32::from_rgb(255, 110, 0))); }
}

fn get_tracks(f: &str) -> Vec<String> { fs::read_dir(f).map(|d| d.flatten().filter_map(|e| { let p = e.path(); let ex = p.extension()?.to_str()?.to_lowercase(); if ["mp3", "wav", "flac"].contains(&ex.as_str()) { Some(e.file_name().to_str()?.to_string()) } else { None } }).collect()).unwrap_or_default() }
fn load_settings() -> Settings { File::open("settings.json").and_then(|f| Ok(serde_json::from_reader(f)?)).unwrap_or(Settings { vol: 0.5, cur_idx: 0, music_folder: "C:/Users/delowar/Music".into(), eq_gains: [0.0; 10] }) }
fn save_settings(s: &Settings) { if let Ok(file) = File::create("settings.json") { let _ = serde_json::to_writer_pretty(file, s); } }
