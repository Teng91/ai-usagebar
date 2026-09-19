//! Small, dependency-free Linux system-resource reader for a separate Waybar
//! module. This is intentionally not part of AI quota rendering: resource use
//! changes on a much shorter cadence and should be configured independently.

#[cfg(target_os = "linux")]
use std::{fs, thread, time::Duration};

use crate::waybar::{Class, WaybarOutput};

#[cfg(target_os = "linux")]
const CPU_SAMPLE_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CpuTicks {
    total: u64,
    idle: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Memory {
    total_kib: u64,
    available_kib: u64,
}

impl Memory {
    fn used_kib(self) -> u64 {
        self.total_kib.saturating_sub(self.available_kib)
    }

    fn used_pct(self) -> u8 {
        percent(self.used_kib(), self.total_kib)
    }
}

/// Produces output compatible with a Waybar custom module. It samples CPU over
/// 100 ms because `/proc/stat` contains cumulative ticks rather than an
/// instantaneous percentage.
pub fn waybar_output() -> WaybarOutput {
    #[cfg(target_os = "linux")]
    {
        match linux_snapshot() {
            Ok((cpu, memory)) => {
                let class = class_for(cpu.max(memory.used_pct()));
                let used_mib = memory.used_kib() / 1024;
                let total_mib = memory.total_kib / 1024;
                WaybarOutput {
                    text: format!("CPU {cpu}% · MEM {}%", memory.used_pct()),
                    tooltip: format!(
                        "<b>System resources</b>\nCPU: {cpu}%\nMemory: {used_mib} MiB / {total_mib} MiB ({}%)",
                        memory.used_pct()
                    ),
                    class,
                }
            }
            Err(message) => unavailable(&message),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unavailable("System usage is currently supported on Linux only.")
    }
}

fn unavailable(message: &str) -> WaybarOutput {
    WaybarOutput {
        text: "SYS unavailable".into(),
        tooltip: message.into(),
        class: Class::Critical,
    }
}

#[cfg(target_os = "linux")]
fn linux_snapshot() -> Result<(u8, Memory), String> {
    let first = read_cpu_ticks()?;
    thread::sleep(CPU_SAMPLE_DELAY);
    let second = read_cpu_ticks()?;
    let memory = parse_memory(&fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?)?;
    let total = second
        .total
        .checked_sub(first.total)
        .ok_or("CPU counters went backwards")?;
    let idle = second
        .idle
        .checked_sub(first.idle)
        .ok_or("CPU idle counter went backwards")?;
    Ok((percent(total.saturating_sub(idle), total), memory))
}

#[cfg(target_os = "linux")]
fn read_cpu_ticks() -> Result<CpuTicks, String> {
    parse_cpu_ticks(&fs::read_to_string("/proc/stat").map_err(|e| e.to_string())?)
}

fn parse_cpu_ticks(input: &str) -> Result<CpuTicks, String> {
    let line = input
        .lines()
        .find(|line| line.starts_with("cpu "))
        .ok_or("missing aggregate CPU counters")?;
    let values: Result<Vec<u64>, _> = line.split_whitespace().skip(1).map(str::parse).collect();
    let values = values.map_err(|_| "invalid CPU counter")?;
    if values.len() < 4 {
        return Err("incomplete CPU counters".into());
    }
    let total = values
        .iter()
        .try_fold(0_u64, |sum, value| sum.checked_add(*value))
        .ok_or("CPU counter overflow")?;
    // Linux reports iowait separately, but it is still time in which no work
    // was executing on that CPU, so include it in idle time.
    let idle = values[3]
        .checked_add(*values.get(4).unwrap_or(&0))
        .ok_or("CPU idle counter overflow")?;
    Ok(CpuTicks { total, idle })
}

fn parse_memory(input: &str) -> Result<Memory, String> {
    let field = |name: &str| -> Result<u64, String> {
        input
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .ok_or_else(|| format!("missing {name}"))?
            .parse()
            .map_err(|_| format!("invalid {name}"))
    };
    let total_kib = field("MemTotal:")?;
    let available_kib = field("MemAvailable:")?;
    if total_kib == 0 || available_kib > total_kib {
        return Err("invalid memory counters".into());
    }
    Ok(Memory {
        total_kib,
        available_kib,
    })
}

fn percent(used: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((used.saturating_mul(100) / total).min(100)) as u8
}

fn class_for(value: u8) -> Class {
    match value {
        0..=59 => Class::Low,
        60..=79 => Class::Mid,
        80..=94 => Class::High,
        _ => Class::Critical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_ticks_and_includes_iowait_as_idle() {
        let ticks = parse_cpu_ticks("cpu  10 20 30 40 5 6 7 8\ncpu0 1 2 3 4\n").unwrap();
        assert_eq!(
            ticks,
            CpuTicks {
                total: 126,
                idle: 45
            }
        );
    }

    #[test]
    fn parses_memory_using_memavailable() {
        let memory = parse_memory("MemTotal:       1000 kB\nMemAvailable:   250 kB\n").unwrap();
        assert_eq!(memory.used_kib(), 750);
        assert_eq!(memory.used_pct(), 75);
    }

    #[test]
    fn percent_and_classes_are_bounded() {
        assert_eq!(percent(200, 100), 100);
        assert_eq!(percent(1, 0), 0);
        assert_eq!(class_for(59), Class::Low);
        assert_eq!(class_for(60), Class::Mid);
        assert_eq!(class_for(80), Class::High);
        assert_eq!(class_for(95), Class::Critical);
    }
}
