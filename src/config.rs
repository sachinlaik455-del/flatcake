//! Single small config file (ARCHITECTURE §10). `key = value` lines, `#` comments.
//! All values bounds-checked at load: unbounded anything is a design violation.

use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Config {
    pub dram_pool_bytes: u64,
    pub dram_high_watermark: f64,
    pub dram_low_watermark: f64,
    pub hbm_window_bytes: u64,
    pub gpu_file_bytes: u64,
    pub gpu_file_pool: u64,
    pub ssd_dir: String,
    pub baseline_ssd_dir: String,
    pub bucket_bytes: u64,
    pub bucket_count: u64,
    pub block_tokens: u64,
    pub bytes_per_token: u64,
    pub tau_tokens: u64,
    pub l_agent_tokens: u64,
    pub gap_bridge_bytes: u64,
    pub ssd_high_watermark: f64,
    pub ssd_low_watermark: f64,
    pub ssd_workers: usize,
    pub dram_workers: usize,
    pub queue_cap: usize,
    pub staging_pool_bytes: u64,
    pub recompute_us_per_token: u64,
    pub index_capacity: u64,
    pub time_scale: f64,
    pub csv_window_us: u64,
}

impl Config {
    pub fn block_bytes(&self) -> u64 {
        self.block_tokens * self.bytes_per_token
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dram_pool_bytes: 536_870_912,
            dram_high_watermark: 0.90,
            dram_low_watermark: 0.70,
            hbm_window_bytes: 67_108_864,
            gpu_file_bytes: 268_435_456,
            gpu_file_pool: 16,
            ssd_dir: "data/fckv".into(),
            baseline_ssd_dir: "data/baseline".into(),
            bucket_bytes: 33_554_432,
            bucket_count: 128,
            block_tokens: 256,
            bytes_per_token: 32,
            tau_tokens: 4096,
            l_agent_tokens: 65536,
            gap_bridge_bytes: 65536,
            ssd_high_watermark: 0.90,
            ssd_low_watermark: 0.70,
            ssd_workers: 2,
            dram_workers: 2,
            queue_cap: 256,
            staging_pool_bytes: 134_217_728,
            recompute_us_per_token: 8,
            index_capacity: 1_048_576,
            time_scale: 1.0,
            csv_window_us: 1_000_000,
        }
    }
}

pub fn load(path: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read config {path}: {e}"))?;
    let mut seen = HashMap::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("config line {}: expected 'key = value', got: {raw}", lineno + 1))?;
        let k = k.trim();
        let v = v.trim();
        seen.insert(k.to_string(), v.to_string());
    }
    let u = |k: &str, cur: &mut u64, min: u64, max: u64| -> Result<(), String> {
        if let Some(v) = seen.get(k) {
            *cur = v.parse::<u64>().map_err(|_| format!("config key {k}: bad u64 '{v}'"))?;
            if *cur < min || *cur > max {
                return Err(format!("config key {k}: {cur} out of bounds [{min}, {max}]"));
            }
        }
        Ok(())
    };
    let f = |k: &str, cur: &mut f64, min: f64, max: f64| -> Result<(), String> {
        if let Some(v) = seen.get(k) {
            *cur = v.parse::<f64>().map_err(|_| format!("config key {k}: bad f64 '{v}'"))?;
            if *cur < min || *cur > max {
                return Err(format!("config key {k}: {cur} out of bounds [{min}, {max}]"));
            }
        }
        Ok(())
    };
    let us = |k: &str, cur: &mut usize, min: usize, max: usize| -> Result<(), String> {
        if let Some(v) = seen.get(k) {
            *cur = v.parse::<usize>().map_err(|_| format!("config key {k}: bad usize '{v}'"))?;
            if *cur < min || *cur > max {
                return Err(format!("config key {k}: {cur} out of bounds [{min}, {max}]"));
            }
        }
        Ok(())
    };
    let s = |k: &str, cur: &mut String| {
        if let Some(v) = seen.get(k) {
            *cur = v.clone();
        }
    };
    u("dram_pool_bytes", &mut cfg.dram_pool_bytes, 1 << 20, 2 << 30)?;
    f("dram_high_watermark", &mut cfg.dram_high_watermark, 0.1, 1.0)?;
    f("dram_low_watermark", &mut cfg.dram_low_watermark, 0.0, 0.99)?;
    u("hbm_window_bytes", &mut cfg.hbm_window_bytes, 1 << 20, 1 << 30)?;
    u("gpu_file_bytes", &mut cfg.gpu_file_bytes, 1 << 20, crate::types::SSD_FILE_MAX)?;
    u("gpu_file_pool", &mut cfg.gpu_file_pool, 1, 1 << 20)?;
    s("ssd_dir", &mut cfg.ssd_dir);
    s("baseline_ssd_dir", &mut cfg.baseline_ssd_dir);
    u("bucket_bytes", &mut cfg.bucket_bytes, 1 << 20, 1 << 32)?;
    u("bucket_count", &mut cfg.bucket_count, 1, 1 << 20)?;
    u("block_tokens", &mut cfg.block_tokens, 1, 1 << 20)?;
    u("bytes_per_token", &mut cfg.bytes_per_token, 1, 4096)?;
    u("tau_tokens", &mut cfg.tau_tokens, 1, 1 << 32)?;
    u("l_agent_tokens", &mut cfg.l_agent_tokens, 1, 1 << 32)?;
    u("gap_bridge_bytes", &mut cfg.gap_bridge_bytes, 0, 1 << 20)?;
    f("ssd_high_watermark", &mut cfg.ssd_high_watermark, 0.1, 1.0)?;
    f("ssd_low_watermark", &mut cfg.ssd_low_watermark, 0.0, 0.99)?;
    us("ssd_workers", &mut cfg.ssd_workers, 1, 64)?;
    us("dram_workers", &mut cfg.dram_workers, 1, 64)?;
    us("queue_cap", &mut cfg.queue_cap, 4, 1 << 20)?;
    u("staging_pool_bytes", &mut cfg.staging_pool_bytes, 1 << 20, 4 << 30)?;
    u("recompute_us_per_token", &mut cfg.recompute_us_per_token, 0, 1_000_000)?;
    u("index_capacity", &mut cfg.index_capacity, 1024, 1 << 26)?;
    f("time_scale", &mut cfg.time_scale, 0.001, 1000.0)?;
    u("csv_window_us", &mut cfg.csv_window_us, 1000, 60_000_000)?;

    if cfg.gpu_file_bytes.count_ones() != 1 {
        return Err("gpu_file_bytes must be a power of two (SSD offset packing)".into());
    }
    if cfg.dram_low_watermark >= cfg.dram_high_watermark
        || cfg.ssd_low_watermark >= cfg.ssd_high_watermark
    {
        return Err("low watermarks must be below high watermarks".into());
    }
    let unknown: Vec<&String> = seen
        .keys()
        .filter(|k| {
            ![
                "dram_pool_bytes", "dram_high_watermark", "dram_low_watermark",
                "hbm_window_bytes", "gpu_file_bytes", "gpu_file_pool", "ssd_dir",
                "baseline_ssd_dir", "bucket_bytes", "bucket_count", "block_tokens",
                "bytes_per_token", "tau_tokens", "l_agent_tokens", "gap_bridge_bytes",
                "ssd_high_watermark", "ssd_low_watermark", "ssd_workers", "dram_workers",
                "queue_cap", "staging_pool_bytes", "recompute_us_per_token",
                "index_capacity", "time_scale", "csv_window_us",
            ]
            .contains(&k.as_str())
        })
        .collect();
    if !unknown.is_empty() {
        return Err(format!("unknown config keys: {unknown:?}"));
    }
    Ok(cfg)
}
