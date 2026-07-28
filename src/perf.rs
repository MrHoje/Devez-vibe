use std::{
    env,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const REPORT_INTERVAL: Duration = Duration::from_secs(5);

struct PerfWindow {
    path: PathBuf,
    started_at: Instant,
    draws: u64,
    animations: u64,
    view_total: Duration,
    render_total: Duration,
    render_max: Duration,
    animation_total: Duration,
    animation_max: Duration,
    live_blocks: usize,
    live_bytes: usize,
}

static PERF: OnceLock<Option<Mutex<PerfWindow>>> = OnceLock::new();

fn perf() -> Option<&'static Mutex<PerfWindow>> {
    PERF.get_or_init(|| {
        let configured = env::var_os("DVZ_PERF")?;
        let path = if configured == "1" {
            env::temp_dir().join(format!("dvz-perf-{}.log", std::process::id()))
        } else {
            PathBuf::from(configured)
        };
        Some(Mutex::new(PerfWindow {
            path,
            started_at: Instant::now(),
            draws: 0,
            animations: 0,
            view_total: Duration::ZERO,
            render_total: Duration::ZERO,
            render_max: Duration::ZERO,
            animation_total: Duration::ZERO,
            animation_max: Duration::ZERO,
            live_blocks: 0,
            live_bytes: 0,
        }))
    })
    .as_ref()
}

pub fn record_draw(
    view_elapsed: Duration,
    render_elapsed: Duration,
    live_blocks: usize,
    live_bytes: usize,
) {
    let Some(perf) = perf() else {
        return;
    };
    let Ok(mut window) = perf.lock() else {
        return;
    };
    window.draws += 1;
    window.view_total += view_elapsed;
    window.render_total += render_elapsed;
    window.render_max = window.render_max.max(render_elapsed);
    window.live_blocks = live_blocks;
    window.live_bytes = live_bytes;
    report_if_due(&mut window);
}

pub fn record_animation(elapsed: Duration) {
    let Some(perf) = perf() else {
        return;
    };
    let Ok(mut window) = perf.lock() else {
        return;
    };
    window.animations += 1;
    window.animation_total += elapsed;
    window.animation_max = window.animation_max.max(elapsed);
    report_if_due(&mut window);
}

fn report_if_due(window: &mut PerfWindow) {
    let elapsed = window.started_at.elapsed();
    if elapsed < REPORT_INTERVAL {
        return;
    }
    let draws = window.draws.max(1);
    let animations = window.animations.max(1);
    let line = format!(
        "window_ms={} draws={} animations={} draws_per_sec={:.1} animation_per_sec={:.1} view_avg_us={} render_avg_us={} render_max_us={} animation_avg_us={} animation_max_us={} live_blocks={} live_kib={}\n",
        elapsed.as_millis(),
        window.draws,
        window.animations,
        window.draws as f64 / elapsed.as_secs_f64(),
        window.animations as f64 / elapsed.as_secs_f64(),
        window.view_total.as_micros() / u128::from(draws),
        window.render_total.as_micros() / u128::from(draws),
        window.render_max.as_micros(),
        window.animation_total.as_micros() / u128::from(animations),
        window.animation_max.as_micros(),
        window.live_blocks,
        window.live_bytes / 1024,
    );
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&window.path)
    {
        let _ = file.write_all(line.as_bytes());
    }
    window.started_at = Instant::now();
    window.draws = 0;
    window.animations = 0;
    window.view_total = Duration::ZERO;
    window.render_total = Duration::ZERO;
    window.render_max = Duration::ZERO;
    window.animation_total = Duration::ZERO;
    window.animation_max = Duration::ZERO;
}
