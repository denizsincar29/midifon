use anyhow::{bail, Result};
use midir::{MidiInput, MidiOutput};

pub fn input_names() -> Result<Vec<String>> {
    let m = MidiInput::new("midifon-list")?;
    Ok(m.ports()
        .iter()
        .map(|p| m.port_name(p).unwrap_or_else(|_| "unknown".into()))
        .collect())
}

pub fn output_names() -> Result<Vec<String>> {
    let m = MidiOutput::new("midifon-list")?;
    Ok(m.ports()
        .iter()
        .map(|p| m.port_name(p).unwrap_or_else(|_| "unknown".into()))
        .collect())
}

/// Выбор устройства по номеру (с 1) или по части имени.
pub fn resolve(names: &[String], query: &str) -> Result<usize> {
    let q = query.trim();
    if let Ok(n) = q.parse::<usize>() {
        if (1..=names.len()).contains(&n) {
            return Ok(n - 1);
        }
        bail!("no device number {n} (there are {})", names.len());
    }
    let ql = q.to_lowercase();
    let hits: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.to_lowercase().contains(&ql))
        .map(|(i, _)| i)
        .collect();
    match hits.len() {
        0 => bail!("no device matches \"{q}\""),
        1 => Ok(hits[0]),
        _ => bail!("\"{q}\" matches {} devices, be more specific or use a number", hits.len()),
    }
}
