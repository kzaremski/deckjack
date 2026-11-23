//! ffmpeg's `-progress pipe:1` stream.
//!
//! `key=value` lines in blocks, each block ending in `progress=continue` or
//! `progress=end`, one block per `-stats_period`. Time is read from
//! `out_time_us`: `out_time_ms` carries microseconds too, despite its name
//! (verified on ffmpeg 9.0.2, lane brief), and a parser that believed the
//! name would report a job a thousand times further along than it is.

/// Where an encode is, after one block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Tick {
    /// Seconds of output written.
    pub out_secs: f64,
    /// Times real time (`2.5` is two and a half seconds of media a second).
    pub speed: Option<f64>,
    /// Bytes written so far.
    pub size: Option<u64>,
    /// The last block: ffmpeg has finished.
    pub end: bool,
}

/// Fed one line at a time; yields a [`Tick`] at the end of each block.
#[derive(Debug, Default)]
pub struct Parser {
    current: Tick,
}

impl Parser {
    pub fn line(&mut self, line: &str) -> Option<Tick> {
        let (key, value) = line.trim().split_once('=')?;
        let value = value.trim();
        match key {
            "out_time_us" => {
                // `N/A` before the first packet; negative at the very start
                // of some streams.
                if let Ok(us) = value.parse::<i64>() {
                    self.current.out_secs = (us.max(0) as f64) / 1_000_000.0;
                }
            }
            "speed" => {
                self.current.speed = value
                    .trim_end_matches('x')
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0);
            }
            "total_size" => self.current.size = value.parse().ok(),
            "progress" => {
                self.current.end = value == "end";
                let tick = self.current;
                return Some(tick);
            }
            _ => {}
        }
        None
    }
}

/// The fraction done, the speed, and the seconds left, from a tick and the
/// length of what is being encoded.
pub fn estimate(tick: &Tick, total_secs: Option<f64>) -> (Option<f64>, Option<f64>) {
    let Some(total) = total_secs.filter(|t| *t > 0.0) else {
        return (None, None);
    };
    let fraction = if tick.end {
        1.0
    } else {
        (tick.out_secs / total).clamp(0.0, 1.0)
    };
    let left = tick
        .speed
        .map(|speed| ((total - tick.out_secs).max(0.0)) / speed);
    (Some(fraction), left)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block as ffmpeg 9.0.2 printed it on this machine.
    const BLOCK: &str = "frame=58\nfps=0.00\nstream_0_0_q=29.0\nbitrate=N/A\ntotal_size=48\n\
                         out_time_us=1933333\nout_time_ms=1933333\nout_time=00:00:01.933333\n\
                         dup_frames=0\ndrop_frames=0\nspeed=3.86x\nprogress=continue\n";

    #[test]
    fn a_block_is_read_by_out_time_us_and_not_by_the_misnamed_ms() {
        let mut p = Parser::default();
        let ticks: Vec<Tick> = BLOCK.lines().filter_map(|l| p.line(l)).collect();
        assert_eq!(ticks.len(), 1);
        let t = ticks[0];
        assert!((t.out_secs - 1.933333).abs() < 1e-6, "{}", t.out_secs);
        assert_eq!(t.speed, Some(3.86));
        assert_eq!(t.size, Some(48));
        assert!(!t.end);
    }

    #[test]
    fn the_last_block_says_it_ended_and_unknowns_stay_unknown() {
        let mut p = Parser::default();
        let text = "out_time_us=N/A\nspeed=N/A\nprogress=continue\n\
                    out_time_us=6000000\nspeed=9.26x\nprogress=end\n";
        let ticks: Vec<Tick> = text.lines().filter_map(|l| p.line(l)).collect();
        assert_eq!(ticks[0].out_secs, 0.0);
        assert_eq!(ticks[0].speed, None);
        assert!(ticks[1].end);
        assert_eq!(ticks[1].out_secs, 6.0);
    }

    #[test]
    fn fraction_and_time_left_follow_speed() {
        let tick = Tick {
            out_secs: 30.0,
            speed: Some(2.0),
            size: None,
            end: false,
        };
        let (fraction, left) = estimate(&tick, Some(120.0));
        assert_eq!(fraction, Some(0.25));
        assert_eq!(left, Some(45.0));
        // Unknown length: no fraction, no estimate, not a division by zero.
        assert_eq!(estimate(&tick, None), (None, None));
        assert_eq!(estimate(&tick, Some(0.0)), (None, None));
        // Past the end (a stream longer than the container said) is done.
        let over = Tick {
            out_secs: 130.0,
            ..tick
        };
        assert_eq!(estimate(&over, Some(120.0)).0, Some(1.0));
    }
}
