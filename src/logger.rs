use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct HourlyLogger {
    dir: PathBuf,
    file: Mutex<Option<(String, File)>>,
}

impl HourlyLogger {
    pub fn new(dir: &str) -> Self {
        let dir = PathBuf::from(dir);
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            file: Mutex::new(None),
        }
    }

    fn hour_key(ts: u64) -> String {
        let secs = ts as i64;
        let days = secs / 86400;
        let hour = (secs % 86400) / 3600;
        let unix_days = days - (1970 * 365 + 492);
        let y400 = unix_days / 146097;
        let rem = unix_days - y400 * 146097;
        let y100 = rem / 36524;
        let rem = rem - y100 * 36524;
        let y4 = rem / 1461;
        let rem = rem - y4 * 1461;
        let y1 = rem / 365;
        let doy = rem - y1 * 365;
        let year = 1970 + y400 * 400 + y100 * 100 + y4 * 4 + y1;
        let month_days = [31, 28 + if is_leap(year) { 1 } else { 0 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut month = 0;
        let mut day = doy as i32;
        for (i, &md) in month_days.iter().enumerate() {
            if day < md {
                month = i + 1;
                break;
            }
            day -= md;
        }
        format!("{:04}-{:02}-{:02}_{:02}", year, month, day + 1, hour)
    }

    pub fn log(&self, line: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let key = Self::hour_key(ts);
        let mut guard = self.file.lock().unwrap();
        match &mut *guard {
            Some((cur_key, f)) if cur_key == &key => {
                let _ = writeln!(f, "{}", line);
            }
            _ => {
                let path = self.dir.join(format!("{}.log", key));
                if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
                    let _ = writeln!(&f, "{}", line);
                    *guard = Some((key, f));
                }
            }
        }
    }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
