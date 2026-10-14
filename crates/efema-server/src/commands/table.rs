//! How the listings print: columns, sizes and times.

/// Columns padded to their widest cell; the ones listed in `right` are
/// numbers and line up on the right.
pub fn print_table<const N: usize>(header: [&str; N], rows: &[[String; N]], right: &[usize]) {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: [&str; N]| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                if right.contains(&i) {
                    format!("{cell:>w$}", w = widths[i])
                } else {
                    format!("{cell:<w$}", w = widths[i])
                }
            })
            .collect();
        println!("{}", padded.join("  ").trim_end());
    };
    line(header);
    for row in rows {
        line(std::array::from_fn(|i| row[i].as_str()));
    }
}

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `YYYY-MM-DD HH:MM` in UTC from seconds since the Unix epoch - the civil
/// calendar from day counts (Howard Hinnant's algorithm), rather than a date
/// crate for one line of a listing.
pub fn timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let minutes = seconds.rem_euclid(86_400) / 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_land_on_the_right_day() {
        assert_eq!(timestamp(0), "1970-01-01 00:00");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00");
        assert_eq!(timestamp(1_791_456_120), "2026-10-08 10:42");
        assert_eq!(timestamp(4_107_542_399), "2100-02-28 23:59");
    }

    #[test]
    fn sizes_read_like_sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KiB");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MiB");
    }
}
