use super::super::*;
use super::common::*;

/// How long one frame of the real app takes, at a fixed splitter position
/// and while the splitter is being dragged:
///
///     cargo test --release -- --ignored --nocapture bench_frame_cost
///
/// The drag is what this exists for: it moves the reading pane's width
/// every frame, which used to re-parse and re-lay out the whole mail body
/// and every row of the message list on each one. It reads the developer's
/// own `mail.db` (`~/.local/share/kimo/`), picks the folder with the most
/// mail and the largest HTML body in it — the worst case a drag can be
/// asked to lay out — and prints medians with the mail open and with
/// nothing selected. A developer tool, not a test: nothing here asserts.
#[test]
#[ignore = "developer tool; prints frame times for a splitter drag and asserts nothing"]
fn bench_frame_cost() {
    let ctx = egui::Context::default();
    let scale = 2.0;
    let Some(mut app) = probe_app(&ctx) else {
        println!("no mail database: nothing to measure");
        return;
    };
    println!(
        "probe: {} / {} / {} messages, body {} KB",
        app.selected_account.as_deref().unwrap_or("-"),
        app.selected_folder.as_deref().unwrap_or("-"),
        app.folder_messages_of(
            app.selected_account.as_deref().unwrap_or_default(),
            app.selected_folder.as_deref().unwrap_or_default()
        )
        .map_or(0, <[MailMessage]>::len),
        app.selected_message_view()
            .and_then(|msg| msg.body_html)
            .map_or(0, |html| html.len() / 1024),
    );

    for open in [true, false] {
        if !open {
            app.selected_message = None;
        }
        // Fixed geometry, then a drag that moves the splitter 8 pt a frame
        // through the list widths a real drag passes over.
        for mode in ["fixed", "drag"] {
            let mut times = Vec::new();
            for step in 0..24 {
                let list = if mode == "fixed" {
                    520.0
                } else {
                    560.0 - step as f32 * 8.0
                };
                let elapsed = probe_frame(&ctx, &mut app, list, scale);
                if step > 2 {
                    times.push(elapsed);
                }
            }
            let median = probe_median(&mut times);
            let max = times.last().copied().unwrap_or_default();
            println!(
                "  {:10}  {:4}  median {:7.2} ms  max {:7.2} ms",
                if open { "mail open" } else { "no mail" },
                mode,
                probe_ms(median),
                probe_ms(max),
            );
        }
    }
}

/// The same question for the navigation column's slide: every frame of the
/// collapse gives the reading pane more width, so a mail open in it is laid
/// out for a new width on each one. The list keeps its own width, and the
/// navigation column itself is never re-laid out: `Panel::show_collapsible`
/// keeps its content at its full width and only slides and clips it.
///
///     cargo test --release -- --ignored --nocapture bench_nav_slide
#[test]
#[ignore = "developer tool; prints frame times for a nav slide and asserts nothing"]
fn bench_nav_slide() {
    let ctx = egui::Context::default();
    let scale = 2.0;
    let Some(mut app) = probe_app(&ctx) else {
        println!("no mail database: nothing to measure");
        return;
    };
    const LIST: f32 = 520.0;
    // 60 frames/s, so the slide is done inside the first 12 frames.
    const MOVING: usize = 12;
    let settle = |app: &mut KimoApp| {
        for _ in 0..24 {
            probe_frame(&ctx, app, LIST, scale);
        }
    };
    let run = |app: &mut KimoApp, count: usize| -> Vec<Duration> {
        (0..count)
            .map(|_| probe_frame(&ctx, app, LIST, scale))
            .collect()
    };
    let report = |label: &str, times: &[Duration]| {
        let mut moving = times[..MOVING.min(times.len())].to_vec();
        let median = probe_median(&mut moving);
        let peak = times.iter().copied().max().unwrap_or_default();
        println!(
            "  {label:12}  moving median {:7.2} ms  peak {:7.2} ms",
            probe_ms(median),
            probe_ms(peak),
        );
    };

    println!(
        "nav slide: {} / {}",
        app.selected_account.as_deref().unwrap_or("-"),
        app.selected_folder.as_deref().unwrap_or("-"),
    );
    for open in [true, false] {
        if !open {
            app.selected_message = None;
        }
        let mail = if open { "mail open" } else { "no mail" };

        app.nav_collapsed = false;
        settle(&mut app);
        let rest = run(&mut app, 16);
        report(&format!("{mail} rest"), &rest);

        app.nav_collapsed = true;
        let out = run(&mut app, 16);
        report(&format!("{mail} slide out"), &out);

        settle(&mut app);
        app.nav_collapsed = false;
        let back = run(&mut app, 16);
        report(&format!("{mail} slide in"), &back);
    }
}
