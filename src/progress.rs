use std::time::Duration;

use indicatif::ProgressStyle;

pub fn steady_tick_duration() -> Duration {
    Duration::from_millis(75)
}

pub fn style_bar() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "{percent:>3}%[{bar:60.cyan/blue}] {bytes:>8}/{total_bytes} {bytes_per_sec} {wide_msg}",
        )
        .unwrap()
        .progress_chars("#|-")
}
