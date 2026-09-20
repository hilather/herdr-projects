//! Shared schedule parsing and explicit civil-time resolution.
use anyhow::{Result, Context, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// `every <N>m|h|d`, in seconds.
    Every(i64),
    /// `daily HH:MM`, local time.
    Daily(i8, i8),
}

pub fn parse_schedule(text: &str) -> Result<Schedule> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("every ") {
        let rest = rest.trim();
        let (digits, seconds) = [('m', 60_i64), ('h', 3600), ('d', 86_400)]
            .into_iter()
            .find_map(|(unit, seconds)| rest.strip_suffix(unit).map(|digits| (digits, seconds)))
            .with_context(|| format!("bad schedule `{text}`: use `every <N>m`, `<N>h` or `<N>d`"))?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            bail!("bad schedule `{text}`: the interval must be positive ASCII decimal digits");
        }
        let n: i64 = digits.parse().ok().filter(|n| *n > 0).with_context(|| format!("bad schedule `{text}`"))?;
        let interval = n.checked_mul(seconds)
            .with_context(|| format!("bad schedule `{text}`: interval exceeds {} seconds", i64::MAX))?;
        return Ok(Schedule::Every(interval));
    }
    if let Some(rest) = text.strip_prefix("daily ") {
        let (h, m) = rest.trim().split_once(':').with_context(|| format!("bad schedule `{text}`"))?;
        let (h, m): (i8, i8) = (h.parse().ok().context("bad hour")?, m.parse().ok().context("bad minute")?);
        if !(0..24).contains(&h) || !(0..60).contains(&m) {
            bail!("bad schedule `{text}`: the time must be 00:00 to 23:59");
        }
        return Ok(Schedule::Daily(h, m));
    }
    bail!("bad schedule `{text}`: use `every <N>m|h|d` or `daily HH:MM`")
}

/// Whether a routine is due, comparing with its stored last run. The time zone
/// is read on each use, so `daily HH:MM` stays right across a daylight-saving
/// change in a long-running ticker.
pub fn is_due(schedule: &Schedule, last_run: jiff::Timestamp, now: &jiff::Zoned) -> bool {
    match schedule {
        Schedule::Every(seconds) => now.timestamp().as_second() - last_run.as_second() >= *seconds,
        Schedule::Daily(hour, minute) => {
            // Resolve the civil time independently of `now`'s UTC offset.
            // Compatible disambiguation shifts gaps forward and chooses the
            // first occurrence of a repeated time, yielding one daily instant.
            let at = |date: jiff::civil::Date| now.time_zone()
                .to_ambiguous_zoned(date.at(*hour, *minute, 0, 0))
                .compatible();
            let mut date=now.date();
            for _ in 0..8 {
                let Ok(candidate)=at(date) else {return false;};
                if candidate.timestamp()<=now.timestamp() {return last_run<candidate.timestamp();}
                let Ok(previous)=date.yesterday() else {return false;};date=previous;
            }
            false
        }
    }
}

/// A bounded summary of calendar/interval slots after a durable cursor. The
/// caller may coalesce to `last_unix_ms` or record the whole window as skipped.
/// No work proportional to downtime is performed, and no execution is authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueWindow {
    pub first_unix_ms: i64,
    pub last_unix_ms: i64,
    pub slots: u64,
}

/// Intervals are anchored to their signed start instant, never to completion.
/// Daily times use an explicit zone and compatible DST resolution: gaps shift
/// forward, repeated times select their first occurrence. Dates skipped by a
/// timezone change can resolve to one instant; durable instant keys deduplicate it.
pub fn due_window(schedule:&Schedule,zone:&jiff::tz::TimeZone,start_ms:i64,after_ms:i64,now_ms:i64)->Result<Option<DueWindow>> {
    anyhow::ensure!(start_ms>=0 && after_ms>=-1 && now_ms>=0,"invalid routine clock");
    let start=jiff::Timestamp::from_millisecond(start_ms)?;
    let now=jiff::Timestamp::from_millisecond(now_ms)?;
    if now_ms<start_ms || now_ms<=after_ms {return Ok(None);}
    match schedule {
        Schedule::Every(seconds)=>{
            anyhow::ensure!(*seconds>0,"interval must be positive");
            let step=i128::from(*seconds)*1000;
            let anchor=i128::from(start_ms);
            let lower=i128::from(after_ms).max(anchor-1)+1;
            let first=anchor+((lower-anchor+step-1)/step)*step;
            let last=anchor+((i128::from(now_ms)-anchor)/step)*step;
            if first>last {return Ok(None);}
            Ok(Some(DueWindow{first_unix_ms:first.try_into()?,last_unix_ms:last.try_into()?,slots:((last-first)/step+1).try_into()?}))
        },
        Schedule::Daily(hour,minute)=>{
            anyhow::ensure!((0..24).contains(hour)&&(0..60).contains(minute),"invalid daily time");
            let at=|date:jiff::civil::Date|->Result<i64>{Ok(zone.to_ambiguous_zoned(date.at(*hour,*minute,0,0)).compatible()?.timestamp().as_millisecond())};
            let lower_ms=start_ms.max(after_ms.checked_add(1).context("routine cursor exhausted")?);
            let lower=jiff::Timestamp::from_millisecond(lower_ms)?.to_zoned(zone.clone());
            let mut first_date=lower.date();
            // A gap spanning midnight can shift a prior civil date into this
            // date. Search the small boundary neighborhood, not the downtime.
            for _ in 0..8 {
                let previous=first_date.yesterday()?;
                if at(previous)?<lower_ms {break;}
                first_date=previous;
            }
            for _ in 0..8 {
                if at(first_date)?>=lower_ms {break;}
                first_date=first_date.tomorrow()?;
            }
            let mut last_date=now.to_zoned(zone.clone()).date();
            for _ in 0..8 {
                if at(last_date)?<=now_ms {break;}
                last_date=last_date.yesterday()?;
            }
            anyhow::ensure!(at(first_date)?>=lower_ms && at(first_date.yesterday()?)?<lower_ms && at(last_date)?<=now_ms,"timezone discontinuity exceeds bounded search");
            if first_date>last_date {return Ok(None);}
            let first=at(first_date)?;let last=at(last_date)?;
            if first>now_ms || last<start.as_millisecond() || first>last {return Ok(None);}
            let slots=u64::try_from(first_date.until(last_date)?.get_days())?+1;
            Ok(Some(DueWindow{first_unix_ms:first,last_unix_ms:last,slots}))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(text:&str)->i64 {text.parse::<jiff::Timestamp>().unwrap().as_millisecond()}
    #[test]
    fn interval_windows_are_anchored_bounded_and_restart_safe() {
        let zone=jiff::tz::TimeZone::UTC;
        let schedule=parse_schedule("every 1m").unwrap();
        let first=due_window(&schedule,&zone,1000,-1,181_001).unwrap().unwrap();
        assert_eq!(first,DueWindow{first_unix_ms:1000,last_unix_ms:181_000,slots:4});
        assert!(due_window(&schedule,&zone,1000,first.last_unix_ms,181_001).unwrap().is_none());
        assert!(due_window(&schedule,&zone,1000,first.last_unix_ms,10_000).unwrap().is_none());
        assert_eq!(due_window(&schedule,&zone,1000,first.last_unix_ms,241_000).unwrap().unwrap().slots,1);
        let centuries=due_window(&schedule,&zone,0,-1,ms("9999-01-01T00:00:00Z")).unwrap().unwrap();
        assert!(centuries.slots>1_000_000_000);
        let huge=Schedule::Every(i64::MAX);
        assert_eq!(due_window(&huge,&zone,0,-1,ms("9999-01-01T00:00:00Z")).unwrap().unwrap().slots,1);
    }
    #[test]
    fn daily_dst_gaps_and_repeats_have_explicit_instants() {
        let zone=jiff::tz::TimeZone::get("America/New_York").unwrap();
        let schedule=parse_schedule("daily 02:30").unwrap();
        let start=ms("2026-03-07T00:00:00Z");
        let window=due_window(&schedule,&zone,start,-1,ms("2026-03-08T07:30:00Z")).unwrap().unwrap();
        assert_eq!(window.slots,2);assert_eq!(window.last_unix_ms,ms("2026-03-08T07:30:00Z"));
        let schedule=parse_schedule("daily 01:30").unwrap();
        let start=ms("2026-11-01T00:00:00Z");
        let first=due_window(&schedule,&zone,start,-1,ms("2026-11-01T05:30:00Z")).unwrap().unwrap();
        assert_eq!(first.slots,1);
        assert!(due_window(&schedule,&zone,start,first.last_unix_ms,ms("2026-11-01T06:30:00Z")).unwrap().is_none());
    }
    #[test]
    fn daily_large_gaps_and_skipped_dates_do_not_replay_an_instant() {
        let zone=jiff::tz::TimeZone::get("Pacific/Apia").unwrap();
        let schedule=parse_schedule("daily 12:00").unwrap();
        let start=ms("2011-12-29T00:00:00Z");let now=ms("2011-12-31T01:00:00Z");
        let early=ms("2011-12-30T11:00:00Z");
        let before_noon=due_window(&schedule,&zone,start,-1,early).unwrap().unwrap();
        assert_eq!(before_noon.last_unix_ms,ms("2011-12-29T22:00:00Z"));
        assert!(due_window(&schedule,&zone,start,before_noon.last_unix_ms,early).unwrap().is_none());
        assert!(!is_due(&schedule,jiff::Timestamp::from_millisecond(before_noon.last_unix_ms).unwrap(),&jiff::Timestamp::from_millisecond(early).unwrap().to_zoned(zone.clone())));
        let window=due_window(&schedule,&zone,start,-1,now).unwrap().unwrap();
        assert!(window.last_unix_ms<=now);
        assert!(due_window(&schedule,&zone,start,window.last_unix_ms,now).unwrap().is_none());
        let long=due_window(&schedule,&jiff::tz::TimeZone::UTC,0,-1,ms("9999-01-01T00:00:00Z")).unwrap().unwrap();
        assert!(long.slots>2_000_000);
    }
    #[test]
    fn daily_windows_match_enumerated_slots_around_timezone_transitions() {
        for name in ["Pacific/Apia","America/New_York","America/Nuuk","America/Santiago","Australia/Lord_Howe"] {
            let zone=jiff::tz::TimeZone::get(name).unwrap();
            for text in ["2011-12-30T11:00:00Z","2023-03-26T00:15:00Z","2026-03-08T07:00:00Z","2026-11-01T06:00:00Z"] {
                let now=ms(text);
                for hour in [0,1,2,12,23] {
                    let schedule=Schedule::Daily(hour,30);let start=now-3*86_400_000;let after=start+86_400_000;
                    let mut date=jiff::Timestamp::from_millisecond(start).unwrap().to_zoned(zone.clone()).date().yesterday().unwrap();
                    let mut instants=Vec::new();
                    for _ in 0..7 {
                        let instant=zone.to_ambiguous_zoned(date.at(hour,30,0,0)).compatible().unwrap().timestamp().as_millisecond();
                        if instant>=start && instant>after && instant<=now {instants.push(instant);}
                        date=date.tomorrow().unwrap();
                    }
                    let window=due_window(&schedule,&zone,start,after,now).unwrap();
                    let expected=instants.first().map(|first|DueWindow{first_unix_ms:*first,last_unix_ms:*instants.last().unwrap(),slots:instants.len() as u64});
                    assert_eq!(window,expected,"{name} {text} {hour}");
                }
            }
        }
    }
}
