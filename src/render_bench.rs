//! Test-only measurement harness for the two mail body renderers.
//!
//!     cargo test --release -- --ignored --nocapture bench_render
//!     KIMO_BENCH_DB=~/.local/share/kimo/mail.db \
//!         cargo test --release -- --ignored --nocapture bench_render
//!
//! The built-in renderer draws a message on *every* frame the reading pane is
//! visible: app.rs calls html::render straight from update, and it re-parses
//! and re-lays out the body each time. It is therefore timed the way the app
//! calls it (inside the pane's ScrollArea) and reported as parse+layout, as
//! the tessellation that follows, and as the frame total. Blitz runs once per
//! (message, pane width) on a worker thread, so it is timed as a single
//! blitz_render::render call and reported with what that produced.
//!
//! A developer tool, not a test: nothing here asserts.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::blitz_render::{self, Cancellation, PageRequest};
use crate::html::{self, HtmlContext};

/// Reading-pane width in points: the app's 460 pt right panel minus its
/// margins and scroll bar. app.rs hands Blitz this value times the pixel
/// scale, quantized to BLITZ_WIDTH_STEP (32) - 864 physical pixels here.
const PANE_WIDTH_PT: f32 = 432.0;
/// Vertical room the pane gives a page; only Blitz's viewport units use it.
const PANE_HEIGHT_PT: f32 = 780.0;
/// Device pixels per point (the development machine runs niri at scale 2).
const SCALE: f32 = 2.0;
/// Measured repetitions per case, after one warm-up run.
const REPS: usize = 7;

/// One body to measure.
struct Case {
    name: String,
    html: String,
    /// Inline cid: parts as (Content-ID, PNG bytes).
    inline: Vec<(String, Vec<u8>)>,
}

impl Case {
    fn kb(&self) -> f64 {
        self.html.len() as f64 / 1024.0
    }
}

/// A short plain mail: what most of a mailbox looks like.
fn simple_mail() -> Case {
    Case {
        name: "simple".to_string(),
        html: "<p>Hi Zhou,</p>\
               <p>The 0.4.18 build passed on <b>all</b> three platforms. The \
               <a href=\"https://example.com/build/418\">build log</a> has the \
               details, and there is nothing for you to do.</p>\
               <p>Regards,<br>Kimo</p>"
            .to_string(),
        inline: Vec::new(),
    }
}

/// Eight digest sections; the bodies are prose-sized, the way a newsletter
/// reads, so the fixture is not just many copies of one paragraph.
const DIGEST: &[(&str, &str)] = &[
    (
        "Faster sync",
        "The IMAP client now fetches headers and bodies in one pass per folder, \
         so a cold mailbox finishes in about half the round trips it used to.",
    ),
    (
        "Blitz rendering",
        "HTML mail can be drawn by a real layout engine as well as by the \
         built-in widget renderer; the choice lives in Preferences.",
    ),
    (
        "Font preferences",
        "Interface and fixed-width families are picked from the fonts installed \
         on the machine, without asking fontconfig for anything.",
    ),
    (
        "Attachment list",
        "Attachments are listed with their names and sizes next to the message \
         body, before the body itself is rendered.",
    ),
    (
        "Cache format",
        "Message caches stay plain JSON under the XDG data directory, and a \
         renamed directory is migrated on first use.",
    ),
    (
        "Tray icon",
        "The status notifier item reconnects to the session bus quietly if the \
         watcher restarts, instead of vanishing after the first timeout.",
    ),
    (
        "Theme detection",
        "The system theme is read from the GTK and KDE configuration files; no \
         subprocess and no bus round trip is involved.",
    ),
    (
        "Protocol work",
        "UID STORE now batches flags per folder, which keeps a large inbox from \
         issuing one command per message.",
    ),
];

/// A newsletter-shaped body: a 600 px layout table, a header with a logo, a
/// hero image, eight digest sections, a 20-row figure table, a 3-column
/// product grid and a footer. Roughly 20 KB - the p90 of the mail cached on
/// this machine (the largest is 97 KB).
fn complex_mail() -> Case {
    let logo = png_bytes(96, 32);
    let hero = png_bytes(600, 180);
    let mut html = String::with_capacity(32 * 1024);
    html.push_str(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Weekly digest</title><style>\
         body { margin: 0; padding: 0; background: #f4f5f7; color: #111827; }\
         .wrap { width: 600px; }\
         h1 { font-size: 22px; margin: 0 0 6px 0; }\
         h2 { font-size: 16px; margin: 18px 0 6px 0; color: #1f2937; }\
         p { font-size: 14px; line-height: 20px; margin: 0 0 10px 0; }\
         .muted { color: #6b7280; font-size: 12px; }\
         .price { font-size: 18px; font-weight: bold; }\
         .btn { display: inline-block; padding: 8px 16px; background: #2563eb; \
                color: #ffffff; text-decoration: none; border-radius: 4px; }\
         .cell { border-bottom: 1px solid #e5e7eb; padding: 6px 10px; font-size: 13px; }\
         </style></head><body>\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\">\
         <tr><td align=\"center\" style=\"padding: 24px 0;\">\
         <table class=\"wrap\" role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" \
                border=\"0\" style=\"background: #ffffff;\">",
    );
    html.push_str(
        "<tr><td style=\"padding: 20px 24px; border-bottom: 1px solid #e5e7eb;\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr>\
         <td width=\"96\" valign=\"middle\"><img src=\"cid:logo\" width=\"96\" height=\"32\" alt=\"Kimo\"></td>\
         <td align=\"right\" class=\"muted\">Week 41 &middot; 2026</td>\
         </tr></table>\
         <h1>What shipped this week</h1>\
         <p class=\"muted\">Eight changes landed, two of them user visible.</p>\
         </td></tr>",
    );
    html.push_str(
        "<tr><td><img src=\"cid:hero\" width=\"600\" height=\"180\" alt=\"Dashboard\"></td></tr>",
    );
    for (index, (title, body)) in DIGEST.iter().enumerate() {
        html.push_str(&format!(
            "<tr><td style=\"padding: 12px 24px;\"><h2>{title}</h2><p>{body}</p>\
             <p class=\"muted\">Read the <a href=\"https://example.com/digest/{index}\">full note</a> \
             ({index} min).</p></td></tr>"
        ));
    }
    html.push_str(
        "<tr><td style=\"padding: 20px 24px;\"><h2>Numbers</h2>\
         <table width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" \
                style=\"border-collapse: collapse;\"><tr>",
    );
    for head in [
        "Region",
        "Sessions",
        "Signups",
        "Retention",
        "p95",
        "Errors",
    ] {
        html.push_str(&format!(
            "<td class=\"cell\" style=\"font-weight: bold;\">{head}</td>"
        ));
    }
    html.push_str("</tr>");
    for row in 0..20 {
        html.push_str("<tr>");
        for column in 0..6 {
            let value = match column {
                0 => format!("Region {}", (b'A' + (row % 8) as u8) as char),
                1 => format!("{}", 12_345 + row * 617),
                2 => format!("{}", 234 + row * 13),
                3 => format!("{}.{}%", 70 + row % 20, row % 10),
                4 => format!("{} ms", 180 + row * 7),
                _ => format!("{}.{}%", row % 3, row % 10),
            };
            html.push_str(&format!("<td class=\"cell\">{value}</td>"));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></td></tr>");
    html.push_str(
        "<tr><td style=\"padding: 4px 24px 20px 24px;\"><h2>Top sellers</h2>\
         <table width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\">",
    );
    for chunk in 0..8 {
        html.push_str("<tr>");
        for column in 0..3 {
            let item = chunk * 3 + column;
            html.push_str(&format!(
                "<td width=\"33%\" valign=\"top\" style=\"padding: 8px 8px 16px 8px; \
                 border: 1px solid #e5e7eb;\">\
                 <p style=\"margin: 0 0 4px 0; font-weight: bold;\">Item {item:02}: widget</p>\
                 <p class=\"price\" style=\"margin: 0 0 8px 0;\">{}.00 USD</p>\
                 <p class=\"muted\" style=\"margin: 0 0 10px 0;\">In stock, ships in 2 days.</p>\
                 <a class=\"btn\" href=\"https://example.com/item/{item}\">Buy</a></td>",
                10 + item * 3,
            ));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></td></tr>");
    html.push_str("<tr><td style=\"padding: 8px 24px 24px 24px;\"><h2>Also on the list</h2><ul>");
    for item in 0..12 {
        html.push_str(&format!(
            "<li style=\"font-size: 14px; line-height: 20px;\">Follow-up {item}: \
             <a href=\"https://example.com/t/{item}\">ticket #{item}</a> is waiting on review.</li>"
        ));
    }
    html.push_str("</ul></td></tr>");
    html.push_str(
        "<tr><td class=\"muted\" style=\"padding: 16px 24px; background: #f9fafb;\">\
         You are receiving this because you subscribed.<br>\
         <a href=\"https://example.com/unsubscribe\">Unsubscribe</a> &middot; \
         <a href=\"https://example.com/preferences\">Email preferences</a></td></tr>",
    );
    html.push_str("</table></td></tr></table></body></html>");
    Case {
        name: "complex".to_string(),
        html,
        inline: vec![("logo".to_string(), logo), ("hero".to_string(), hero)],
    }
}

/// A small PNG, the way inline images are stored, so both renderers get the
/// same bytes the app would hand them.
fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 190, 255])
    });
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode png");
    png
}

/// Real mail from the mail database, when KIMO_BENCH_DB names a `mail.db`: the
/// median-sized and the largest HTML body, so the fixtures above can be checked
/// against what this client actually receives.
fn cached_cases() -> Vec<Case> {
    let Some(path) = std::env::var_os("KIMO_BENCH_DB") else {
        return Vec::new();
    };
    let conn = rusqlite::Connection::open(&path).expect("open the mail database");
    let rows: Vec<(i64, String, String)> = conn
        .prepare(
            "SELECT id, subject, body_html FROM messages
              WHERE body_html IS NOT NULL ORDER BY length(body_html)",
        )
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .filter_map(Result::ok)
        .collect();
    if rows.is_empty() {
        println!("KIMO_BENCH_DB has no HTML bodies");
        return Vec::new();
    }
    let mut picked: Vec<&(i64, String, String)> = vec![&rows[rows.len() / 2]];
    if let Some(largest) = rows.last() {
        picked.push(largest);
    }
    let mut seen = HashSet::new();
    picked
        .into_iter()
        .filter(|row| seen.insert(row.0))
        .map(|(id, subject, html)| {
            let kb = html.len() as f64 / 1024.0;
            let subject: String = subject.chars().take(24).collect();
            Case {
                name: format!("cache {kb:.1} KB  {subject}"),
                html: html.clone(),
                inline: inline_images(&conn, *id),
            }
        })
        .collect()
}

/// The inline `cid:` parts of one stored message, as the renderer wants them.
fn inline_images(conn: &rusqlite::Connection, message_id: i64) -> Vec<(String, Vec<u8>)> {
    conn.prepare("SELECT cid, data FROM inline_images WHERE message_id = ?1 ORDER BY cid")
        .expect("prepare")
        .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .filter_map(Result::ok)
        .collect()
}

#[derive(Default)]
struct SimpleStats {
    layout: Vec<Duration>,
    tessellate: Vec<Duration>,
    frame: Vec<Duration>,
    vertices: usize,
}

#[derive(Default)]
struct BlitzStats {
    render: Vec<Duration>,
    /// The first render of the process, which also builds Blitz's font
    /// context from the curated on-disk set.
    first: Duration,
    css: (f32, f32),
    raster: (u32, u32),
    tiles: usize,
    links: usize,
    scale: f32,
    truncated: bool,
}

/// Time the built-in renderer the way app.rs drives it: inside the pane's
/// ScrollArea, once per frame, with the message's inline images as textures.
fn bench_simple(ctx: &egui::Context, case: &Case) -> SimpleStats {
    let inline = inline_textures(ctx, case);
    let images = HashMap::new();
    let sheets = HashMap::new();
    let mut stats = SimpleStats::default();
    for rep in 0..=REPS {
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(PANE_WIDTH_PT + 10.0, PANE_HEIGHT_PT),
            )),
            ..Default::default()
        };
        let mut layout = Duration::ZERO;
        let frame_start = Instant::now();
        let mut output = ctx.run_ui(input(), |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                            let mut html_ctx = HtmlContext {
                                images: &images,
                                inline_images: &inline,
                                stylesheets: &sheets,
                                missing_images: Vec::new(),
                                missing_stylesheets: Vec::new(),
                            };
                            let start = Instant::now();
                            html::render(ui, &case.html, &mut html_ctx);
                            layout += start.elapsed();
                        });
                });
        });
        // epaint panics on a dropped `TexturesDelta` (egui 0.36), and this
        // bench only wants the shapes.
        output.textures_delta.clear();
        let frame = frame_start.elapsed();
        let tessellate_start = Instant::now();
        let primitives = ctx.tessellate(output.shapes, SCALE);
        let tessellate = tessellate_start.elapsed();
        if rep == 0 {
            continue; // warm-up: font atlas, galley cache, first paint
        }
        stats.vertices = primitives
            .iter()
            .map(|primitive| match &primitive.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                egui::epaint::Primitive::Callback(_) => 0,
            })
            .sum();
        stats.layout.push(layout);
        stats.tessellate.push(tessellate);
        stats.frame.push(frame);
    }
    stats
}

/// Time one full Blitz render, exactly as the reading pane's worker does.
fn bench_blitz(case: &Case) -> BlitzStats {
    let request = || PageRequest {
        html: case.html.clone(),
        width: (PANE_WIDTH_PT * SCALE).round() as u32,
        viewport_height: (PANE_HEIGHT_PT * SCALE).round() as u32,
        scale: SCALE,
        inline_images: case.inline.clone(),
        ticket: Cancellation::default().issue(),
    };
    let mut stats = BlitzStats::default();
    let mut page = None;
    for rep in 0..=REPS {
        let start = Instant::now();
        let rendered = blitz_render::render(&request())
            .expect("render")
            .expect("page");
        let elapsed = start.elapsed();
        if rep == 0 {
            stats.first = elapsed;
        } else {
            stats.render.push(elapsed);
        }
        page = Some(rendered);
    }
    let page = page.expect("a page");
    stats.css = (page.css_width, page.css_height);
    stats.tiles = page.tiles.len();
    stats.links = page.links.len();
    stats.scale = page.scale;
    stats.truncated = page.truncated;
    let raster_height: u32 = page.tiles.iter().map(|tile| tile.height).sum();
    stats.raster = (
        page.tiles.first().map(|tile| tile.width).unwrap_or(0),
        raster_height,
    );
    stats
}

/// The inline images a message carries, as egui textures (app.rs decodes them
/// once per message, so this is not part of the measured frame either).
fn inline_textures(ctx: &egui::Context, case: &Case) -> HashMap<String, egui::TextureHandle> {
    case.inline
        .iter()
        .filter_map(|(cid, data)| {
            let decoded = image::load_from_memory(data).ok()?.to_rgba8();
            let size = [decoded.width() as usize, decoded.height() as usize];
            let pixels = egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw());
            let key = format!("bench-{cid}");
            Some((
                cid.clone(),
                ctx.load_texture(key, pixels, Default::default()),
            ))
        })
        .collect()
}

/// (median, min, max) in milliseconds.
fn spread(values: &[Duration]) -> (f64, f64, f64) {
    let mut millis: Vec<f64> = values
        .iter()
        .map(|value| value.as_secs_f64() * 1000.0)
        .collect();
    millis.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        millis[millis.len() / 2],
        millis[0],
        millis[millis.len() - 1],
    )
}

/// "median (min-max)", with the unit in the column header.
fn cell(values: &[Duration]) -> String {
    let (median, min, max) = spread(values);
    format!("{median:.2} ({min:.2}-{max:.2})")
}

fn megabytes(width: u32, height: u32) -> f64 {
    f64::from(width) * f64::from(height) * 4.0 / (1024.0 * 1024.0)
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|head| head.len()).collect();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    let line = |cells: Vec<String>| {
        let last = cells.len() - 1;
        let mut text = String::new();
        for (index, value) in cells.iter().enumerate() {
            if index == last {
                text.push_str(value);
            } else {
                text.push_str(&format!("{:<width$}  ", value, width = widths[index]));
            }
        }
        println!("{text}");
    };
    line(headers.iter().map(|head| head.to_string()).collect());
    for row in rows {
        line(row.clone());
    }
}

/// The CPU model, so a set of numbers can be attributed to a machine.
fn cpu_model() -> Option<String> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let line = text.lines().find(|line| line.starts_with("model name"))?;
    line.split_once(':')
        .map(|(_, value)| value.trim().to_string())
}

#[test]
#[ignore = "developer tool; prints timings and asserts nothing"]
fn bench_render() {
    let mut cases = vec![simple_mail(), complex_mail()];
    cases.extend(cached_cases());

    println!(
        "Kimo mail render measurement - {} {}, {} threads",
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug (unoptimized)"
        } else {
            "release"
        },
        std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
    );
    if let Some(cpu) = cpu_model() {
        println!("CPU: {cpu}");
    }
    println!(
        "pane {}x{} pt at {SCALE} px/pt ({} px wide), {REPS} reps after a warm-up",
        PANE_WIDTH_PT,
        PANE_HEIGHT_PT,
        (PANE_WIDTH_PT * SCALE).round(),
    );
    if std::env::var_os("KIMO_BENCH_DB").is_none() {
        println!("(set KIMO_BENCH_DB=<mail.db> to add real cached mail)");
    }
    println!();

    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(SCALE);
    crate::fonts::install(&ctx, None, None);

    let mut simple_rows = Vec::new();
    let mut blitz_rows = Vec::new();
    let mut blitz_first_case = String::new();
    let mut blitz_first_ms = 0.0_f64;
    for case in &cases {
        let simple = bench_simple(&ctx, case);
        simple_rows.push(vec![
            case.name.clone(),
            format!("{:.1} KB", case.kb()),
            cell(&simple.layout),
            cell(&simple.tessellate),
            cell(&simple.frame),
            format!("{}", simple.vertices),
        ]);
        let blitz = bench_blitz(case);
        if blitz_first_case.is_empty() {
            blitz_first_case = case.name.clone();
            blitz_first_ms = blitz.first.as_secs_f64() * 1000.0;
        }
        blitz_rows.push(vec![
            case.name.clone(),
            format!("{:.1} KB", case.kb()),
            cell(&blitz.render),
            format!("{:.0}x{:.0}", blitz.css.0, blitz.css.1),
            format!("{:.0}x{:.0}", blitz.raster.0, blitz.raster.1),
            format!("{:.1} MB", megabytes(blitz.raster.0, blitz.raster.1)),
            format!("{}", blitz.tiles),
            format!("{}", blitz.links),
            format!("{:.2}", blitz.scale),
            if blitz.truncated { "yes" } else { "no" }.to_string(),
        ]);
    }

    println!("built-in (egui) renderer, per frame - it re-renders every frame the pane is visible");
    print_table(
        &[
            "case",
            "html",
            "parse+layout ms",
            "tessellate ms",
            "frame total ms",
            "vertices",
        ],
        &simple_rows,
    );
    println!();
    println!("Blitz renderer, per render - once per (message, pane width), on a worker thread");
    print_table(
        &[
            "case",
            "html",
            "render ms",
            "page pt",
            "raster px",
            "raster",
            "tiles",
            "links",
            "scale",
            "cut",
        ],
        &blitz_rows,
    );
    println!();
    println!(
        "notes: the first Blitz render of this process was the {} case and took {:.0} ms, \
         which includes building Blitz's font context from the curated on-disk set; every \
         later render in the table above is steady state. The built-in renderer pays its \
         parse+layout on every painted frame (size-dependent, ~1.3 ms for the 100 KB mail), \
         Blitz once per (message, pane width) and then only the texture memory listed.",
        blitz_first_case, blitz_first_ms,
    );
}
