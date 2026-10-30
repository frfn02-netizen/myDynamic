use std::fs;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use crate::core::{AppEvent, EventSender, SystemStatus};

const DEFAULT_INTERVAL_MS: u64 = 2_000;
const MIN_INTERVAL_MS: u64 = 500;
const MAX_INTERVAL_MS: u64 = 10_000;
const INITIAL_SAMPLE_MS: u64 = 250;

pub fn spawn(tx: EventSender, control: Receiver<bool>) {
    let result = thread::Builder::new()
        .name("glass-island-monitor".into())
        .spawn(move || run(tx, control));

    if let Err(error) = result {
        warn!(%error, "failed to start system monitor");
    }
}

fn run(tx: EventSender, control: Receiver<bool>) {
    let interval = monitor_interval();
    let mut expanded = false;

    loop {
        if !expanded {
            match control.recv() {
                Ok(value) => expanded = value,
                Err(_) => return,
            }
            if !expanded {
                continue;
            }

            // Take a short baseline so the first update arrives quickly.
            if let Some(mut previous) = read_cpu() {
                thread::sleep(Duration::from_millis(INITIAL_SAMPLE_MS));
                if let Some(current) = read_cpu() {
                    emit_sample(&tx, previous, current);
                    previous = current;
                }
                loop {
                    match control.recv_timeout(Duration::from_millis(interval)) {
                        Ok(false) => {
                            expanded = false;
                            break;
                        }
                        Ok(true) => {}
                        Err(RecvTimeoutError::Timeout) => {
                            if let Some(current) = read_cpu() {
                                emit_sample(&tx, previous, current);
                                previous = current;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            } else {
                // CPU is unavailable; still stop sampling until reactivated.
                loop {
                    match control.recv() {
                        Ok(false) | Err(_) => {
                            expanded = false;
                            break;
                        }
                        Ok(true) => {}
                    }
                }
            }
        } else {
            // Defensive: if we entered with expanded=true, wait for control.
            match control.recv() {
                Ok(value) => expanded = value,
                Err(_) => return,
            }
        }
    }
}

fn emit_sample(tx: &EventSender, previous: (u64, u64), current: (u64, u64)) {
    let cpu = cpu_usage(previous, current).unwrap_or(0.0);
    let Some(ram) = read_ram() else {
        return;
    };

    debug!(cpu, ram, "system sample");
    let _ = tx.send(AppEvent::System(Some(SystemStatus { cpu, ram })));
}

fn monitor_interval() -> u64 {
    std::env::var("GLASS_ISLAND_MONITOR_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

fn read_cpu() -> Option<(u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    parse_cpu_line(&stat)
}

fn read_ram() -> Option<f32> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&meminfo)
}

fn parse_cpu_line(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let mut values = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok());
    let mut total = 0u64;
    let mut idle = 0u64;

    for (index, value) in values.by_ref().enumerate() {
        total = total.saturating_add(value);
        if index == 3 || index == 4 {
            idle = idle.saturating_add(value);
        }
    }

    (total > 0).then_some((total, idle))
}

fn cpu_usage(previous: (u64, u64), current: (u64, u64)) -> Option<f32> {
    let total_delta = current.0.checked_sub(previous.0)?;
    if total_delta == 0 {
        return Some(0.0);
    }
    let idle_delta = current.1.saturating_sub(previous.1).min(total_delta);
    Some((1.0 - idle_delta as f32 / total_delta as f32).clamp(0.0, 1.0))
}

fn parse_meminfo(meminfo: &str) -> Option<f32> {
    let mut total = None;
    let mut available = None;

    for line in meminfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.split_whitespace().next()?.parse::<u64>().ok()?;
        match key {
            "MemTotal" => total = Some(value),
            "MemAvailable" => available = Some(value),
            _ => {}
        }
    }

    let total = total?;
    let available = available?;
    if total == 0 {
        return None;
    }
    Some((1.0 - available as f32 / total as f32).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_line_totals_and_idle_are_parsed() {
        let sample = "cpu  100 0 50 850 0 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(parse_cpu_line(sample), Some((1000, 850)));
    }

    #[test]
    fn cpu_usage_uses_delta_and_is_clamped() {
        let usage = cpu_usage((100, 80), (200, 170)).expect("usage");
        assert!((usage - 0.1).abs() < 1e-6);
        assert_eq!(cpu_usage((100, 80), (200, 80)), Some(1.0));
        assert_eq!(cpu_usage((100, 80), (100, 80)), Some(0.0));
        assert_eq!(cpu_usage((100, 80), (200, 250)), Some(0.0));
    }

    #[test]
    fn meminfo_usage_is_parsed() {
        let sample = "MemTotal:       1000 kB\nMemFree: 100 kB\nMemAvailable:    250 kB\n";
        assert_eq!(parse_meminfo(sample), Some(0.75));
    }

    #[test]
    fn monitor_interval_constants_are_sane() {
        const { assert!(MIN_INTERVAL_MS <= DEFAULT_INTERVAL_MS) };
        const { assert!(DEFAULT_INTERVAL_MS <= MAX_INTERVAL_MS) };
    }
}
