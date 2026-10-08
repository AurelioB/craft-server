//! Logging to stderr (container logs, the default) and optionally to a size-rotated file in
//! LOG_DIR, created under the configured identity and umask like every other server-side write.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;

use crate::fsutil::FILE_MODE;
use crate::timeutil::now_iso;

struct FileSink {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    backups: u32,
}

impl FileSink {
    fn rotate(&mut self) -> std::io::Result<()> {
        for i in (1..self.backups).rev() {
            let from = self.path.with_extension(format!("log.{i}"));
            if from.exists() {
                fs::rename(&from, self.path.with_extension(format!("log.{}", i + 1)))?;
            }
        }
        if self.backups > 0 {
            fs::rename(&self.path, self.path.with_extension("log.1"))?;
        } else {
            fs::remove_file(&self.path)?;
        }
        self.file = open(&self.path)?;
        Ok(())
    }
}

fn open(path: &PathBuf) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .open(path)
}

struct Logger {
    level: LevelFilter,
    file: Option<Mutex<FileSink>>,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= self.level && m.target().starts_with("craft_updater")
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let level = match r.level() {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        let line = format!("{} {level} {}\n", now_iso(), r.args());
        let _ = std::io::stderr().write_all(line.as_bytes());
        if let Some(sink) = &self.file {
            let mut s = sink.lock();
            let _ = s.file.write_all(line.as_bytes());
            if s.file
                .metadata()
                .map(|m| m.len() > s.max_bytes)
                .unwrap_or(false)
            {
                let _ = s.rotate();
            }
        }
    }

    fn flush(&self) {}
}

pub fn init(log_dir: Option<&PathBuf>, max_bytes: u64, backups: u32) {
    let level = match std::env::var("CRAFT_LOG_LEVEL")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };
    let file = log_dir.and_then(|d| {
        let path = d.join("updater.log");
        match open(&path) {
            Ok(file) => Some(Mutex::new(FileSink {
                path,
                file,
                max_bytes,
                backups,
            })),
            Err(e) => {
                eprintln!(
                    "{} warn cannot open {}: {e}; logging to stderr only",
                    now_iso(),
                    path.display()
                );
                None
            }
        }
    });
    let _ = log::set_boxed_logger(Box::new(Logger { level, file }));
    log::set_max_level(level);
}
