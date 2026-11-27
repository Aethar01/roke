use crate::config::normalize_device_tag;
use crate::model::{
    civil_from_days, days_from_civil, now_millis, parse_due_date, parse_due_time, parse_tags,
    Priority, Status, Task,
};

/// Unfold RFC 5545 folded lines (`CRLF`/`LF` followed by a single space or horizontal tab).
pub fn unfold_lines(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.replace("\r\n", "\n").split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cont) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(cont);
                continue;
            }
        }
        if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }
    lines
}

/// Fold an RFC 5545 content line at 75 octets using `CRLF` + single space, preserving UTF-8 boundaries.
pub fn fold_line(line: &str) -> String {
    if line.len() <= 75 {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + line.len() / 70 * 3);
    let mut rest = line;
    let mut limit = 75;
    let mut first = true;
    while !rest.is_empty() {
        if !first {
            out.push_str("\r\n ");
            limit = 74;
        }
        first = false;
        if rest.len() <= limit {
            out.push_str(rest);
            break;
        }
        let mut split = limit;
        while split > 0 && !rest.is_char_boundary(split) {
            split -= 1;
        }
        if split == 0 {
            split = rest
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(rest.len());
        }
        out.push_str(&rest[..split]);
        rest = &rest[split..];
    }
    out
}

pub fn escape_ics_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

pub fn unescape_ics_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

pub type ParsedIcsLine = (String, Vec<(String, String)>, String);

/// Split an unfolded content line into `(NAME_UPPER, params, value)`.
pub fn parse_ics_line(line: &str) -> Option<ParsedIcsLine> {
    let mut in_quotes = false;
    let mut colon_idx = None;
    for (idx, ch) in line.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon_idx = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let colon = colon_idx?;
    let head = &line[..colon];
    let value = line[colon + 1..].to_string();

    let mut parts = Vec::new();
    let mut cur_start = 0;
    in_quotes = false;
    for (idx, ch) in head.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&head[cur_start..idx]);
                cur_start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&head[cur_start..]);

    let name = parts.first()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let mut params = Vec::new();
    for p in parts.iter().skip(1) {
        if let Some((k, v)) = p.split_once('=') {
            params.push((
                k.trim().to_ascii_uppercase(),
                v.trim().trim_matches('"').to_string(),
            ));
        }
    }
    Some((name, params, value))
}

fn split_unescaped_commas(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            cur.push('\\');
            cur.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == ',' {
            out.push(unescape_ics_text(&cur));
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if escaped {
        cur.push('\\');
    }
    if !cur.is_empty() {
        out.push(unescape_ics_text(&cur));
    }
    out
}

pub fn format_utc_timestamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let h = sod / 3600;
    let min = (sod % 3600) / 60;
    let s = sod % 60;
    format!("{y:04}{m:02}{d:02}T{h:02}{min:02}{s:02}Z")
}

pub fn parse_ics_timestamp_ms(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 8 {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    let days = days_from_civil(y, m, d);

    if s.len() >= 15 && (s.as_bytes()[8] == b'T' || s.as_bytes()[8] == b't') {
        let h: i64 = s[9..11].parse().ok()?;
        let min: i64 = s[11..13].parse().ok()?;
        let sec: i64 = s[13..15].parse().ok()?;
        let secs = days * 86_400 + h * 3600 + min * 60 + sec;
        return u64::try_from(secs).ok().map(|v| v * 1000);
    }

    let secs = days * 86_400;
    u64::try_from(secs).ok().map(|v| v * 1000)
}

#[cfg(unix)]
fn utc_secs_to_local_ymd_hm(secs: i64) -> (i64, u32, u32, u32, u32) {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ptr = unsafe { libc::localtime_r(&t, &mut tm) };
    if ptr.is_null() {
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        return (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32);
    }
    (
        (tm.tm_year as i64) + 1900,
        (tm.tm_mon as u32) + 1,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

#[cfg(not(unix))]
fn utc_secs_to_local_ymd_hm(secs: i64) -> (i64, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, (sod / 3600) as u32, ((sod % 3600) / 60) as u32)
}

#[cfg(unix)]
fn local_ymd_hm_to_utc_secs(y: i64, m: u32, d: u32, h: u32, min: u32) -> i64 {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (y - 1900) as libc::c_int;
    tm.tm_mon = (m - 1) as libc::c_int;
    tm.tm_mday = d as libc::c_int;
    tm.tm_hour = h as libc::c_int;
    tm.tm_min = min as libc::c_int;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let t = unsafe { libc::mktime(&mut tm) };
    if t == -1 {
        days_from_civil(y, m, d) * 86_400 + (h as i64) * 3600 + (min as i64) * 60
    } else {
        t as i64
    }
}

#[cfg(not(unix))]
fn local_ymd_hm_to_utc_secs(y: i64, m: u32, d: u32, h: u32, min: u32) -> i64 {
    days_from_civil(y, m, d) * 86_400 + (h as i64) * 3600 + (min as i64) * 60
}

fn format_due_line(due: &str, due_time: Option<&str>) -> Option<String> {
    let normalized = parse_due_date(due).ok()?;
    let compact = normalized.replace('-', "");
    if let Some(time_raw) = due_time {
        if let Ok(hm) = parse_due_time(time_raw) {
            let y: i64 = normalized[0..4].parse().ok()?;
            let m: u32 = normalized[5..7].parse().ok()?;
            let d: u32 = normalized[8..10].parse().ok()?;
            let h: u32 = hm[0..2].parse().ok()?;
            let min: u32 = hm[3..5].parse().ok()?;
            let utc_secs = local_ymd_hm_to_utc_secs(y, m, d, h, min);
            let ms = u64::try_from(utc_secs).ok()? * 1000;
            return Some(format!("DUE:{}", format_utc_timestamp(ms)));
        }
    }
    Some(format!("DUE;VALUE=DATE:{compact}"))
}

fn parse_due_property(
    params: &[(String, String)],
    value: &str,
) -> (Option<String>, Option<String>) {
    let val = value.trim();
    let is_date_only = params
        .iter()
        .any(|(k, v)| k == "VALUE" && v.eq_ignore_ascii_case("DATE"))
        || (val.len() == 8 && val.chars().all(|c| c.is_ascii_digit()));

    if is_date_only && val.len() >= 8 {
        let candidate = format!("{}-{}-{}", &val[0..4], &val[4..6], &val[6..8]);
        if let Ok(date) = parse_due_date(&candidate) {
            return (Some(date), None);
        }
        return (None, None);
    }

    if val.len() >= 15 && (val.as_bytes()[8] == b'T' || val.as_bytes()[8] == b't') {
        let is_utc = val.ends_with('Z') || val.ends_with('z');
        if is_utc {
            if let Some(ms) = parse_ics_timestamp_ms(val) {
                let (y, m, d, h, min) = utc_secs_to_local_ymd_hm((ms / 1000) as i64);
                return (
                    Some(format!("{y:04}-{m:02}-{d:02}")),
                    Some(format!("{h:02}:{min:02}")),
                );
            }
        } else {
            let candidate = format!("{}-{}-{}", &val[0..4], &val[4..6], &val[6..8]);
            let time_cand = format!("{}:{}", &val[9..11], &val[11..13]);
            if let Ok(date) = parse_due_date(&candidate) {
                let time = parse_due_time(&time_cand).ok();
                return (Some(date), time);
            }
        }
    }

    (None, None)
}

/// Convert a roke recurrence rule to an RFC 5545 `RRULE` value.
pub fn repeat_to_rrule(repeat: &str) -> Option<String> {
    match repeat.trim() {
        "daily" => Some("FREQ=DAILY".into()),
        "weekdays" => Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".into()),
        "weekly" => Some("FREQ=WEEKLY".into()),
        "monthly" => Some("FREQ=MONTHLY".into()),
        other => {
            let body = other.strip_prefix("every ")?;
            if let Some(n) = body.strip_suffix('d').and_then(|s| s.parse::<u32>().ok()) {
                Some(format!("FREQ=DAILY;INTERVAL={n}"))
            } else if let Some(n) = body.strip_suffix('w').and_then(|s| s.parse::<u32>().ok()) {
                Some(format!("FREQ=WEEKLY;INTERVAL={n}"))
            } else {
                body.strip_suffix('m')
                    .and_then(|s| s.parse::<u32>().ok())
                    .map(|n| format!("FREQ=MONTHLY;INTERVAL={n}"))
            }
        }
    }
}

/// Convert an RFC 5545 `RRULE` value to a roke recurrence string if representable losslessly.
/// Returns `None` for complex rules (e.g. `COUNT=`, `UNTIL=`, `BYMONTHDAY=`) so we preserve them.
pub fn rrule_to_repeat(rrule: &str) -> Option<String> {
    let mut freq: Option<String> = None;
    let mut interval: u32 = 1;
    let mut byday: Option<String> = None;

    for part in rrule.trim().split(';') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=')?;
        match k.trim().to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(v.trim().to_ascii_uppercase()),
            "INTERVAL" => interval = v.trim().parse().ok()?,
            "BYDAY" => byday = Some(v.trim().to_ascii_uppercase()),
            // Preserve unknown/richer RRULE parts unmodified instead of flattening them
            "WKST" => {}
            _ => return None,
        }
    }

    match (freq.as_deref(), interval, byday.as_deref()) {
        (Some("DAILY"), 1, None) => Some("daily".into()),
        (Some("DAILY"), n, None) if n > 1 => Some(format!("every {n}d")),
        (Some("WEEKLY"), 1, Some(days)) => {
            let mut list: Vec<&str> = days.split(',').map(str::trim).collect();
            list.sort_unstable();
            if list == ["FR", "MO", "TH", "TU", "WE"] {
                Some("weekdays".into())
            } else {
                None
            }
        }
        (Some("WEEKLY"), 1, None) => Some("weekly".into()),
        (Some("WEEKLY"), n, None) if n > 1 => Some(format!("every {n}w")),
        (Some("MONTHLY"), 1, None) => Some("monthly".into()),
        (Some("MONTHLY"), n, None) if n > 1 => Some(format!("every {n}m")),
        _ => None,
    }
}

fn priority_to_ics(p: Priority) -> u8 {
    match p {
        Priority::High => 1,
        Priority::Medium => 5,
        Priority::Low => 9,
    }
}

fn ics_to_priority(val: &str) -> Priority {
    match val.trim().parse::<u8>().unwrap_or(0) {
        1..=4 => Priority::High,
        6..=9 => Priority::Low,
        _ => Priority::Medium,
    }
}

fn status_to_ics(s: Status) -> &'static str {
    match s {
        Status::Todo => "NEEDS-ACTION",
        Status::InProgress => "IN-PROCESS",
        Status::Done => "COMPLETED",
    }
}

fn ics_to_status(val: &str) -> Status {
    match val.trim().to_ascii_uppercase().as_str() {
        "IN-PROCESS" => Status::InProgress,
        "COMPLETED" | "CANCELLED" => Status::Done,
        _ => Status::Todo,
    }
}

/// Parse an RFC 5545 `VCALENDAR` containing a `VTODO` into a `roke` [`Task`].
pub fn vtodo_to_task(ics: &str, fallback_origin: &str) -> Result<Task, String> {
    let lines = unfold_lines(ics);
    let mut in_vtodo = false;
    let mut sub_depth: usize = 0;
    let mut saw_vtodo = false;

    let mut uid = String::new();
    let mut title = String::new();
    let mut description = String::new();
    let mut status = Status::Todo;
    let mut priority = Priority::Medium;
    let mut tags: Vec<String> = Vec::new();
    let mut due: Option<String> = None;
    let mut due_time: Option<String> = None;
    let mut repeat: Option<String> = None;
    let mut created_at: Option<u64> = None;
    let mut completed_at: Option<u64> = None;
    let mut last_modified_ms: Option<u64> = None;
    let mut dtstamp_ms: Option<u64> = None;
    let mut roke_updated_ms: Option<u64> = None;
    let mut origin: Option<String> = None;
    let mut order: Option<i64> = None;
    let mut apple_order: Option<i64> = None;

    for line in &lines {
        let Some((name, params, value)) = parse_ics_line(line) else {
            continue;
        };
        if name == "BEGIN" {
            let comp = value.trim().to_ascii_uppercase();
            if comp == "VTODO" && !in_vtodo {
                in_vtodo = true;
                saw_vtodo = true;
                continue;
            } else if in_vtodo {
                sub_depth += 1;
                continue;
            }
        } else if name == "END" {
            let comp = value.trim().to_ascii_uppercase();
            if in_vtodo {
                if sub_depth > 0 {
                    sub_depth -= 1;
                } else if comp == "VTODO" {
                    break;
                }
                continue;
            }
        }

        if !in_vtodo || sub_depth > 0 {
            continue;
        }

        match name.as_str() {
            "UID" => uid = value.trim().to_string(),
            "SUMMARY" => title = unescape_ics_text(&value).trim().to_string(),
            "DESCRIPTION" => description = unescape_ics_text(&value),
            "STATUS" => status = ics_to_status(&value),
            "PRIORITY" => priority = ics_to_priority(&value),
            "CATEGORIES" => {
                for raw_cat in split_unescaped_commas(&value) {
                    for tag in parse_tags(&raw_cat) {
                        if !tags.contains(&tag) {
                            tags.push(tag);
                        }
                    }
                }
            }
            "DUE" => {
                let (d, t) = parse_due_property(&params, &value);
                due = d;
                due_time = t;
            }
            "RRULE" => {
                repeat = rrule_to_repeat(&value);
            }
            "CREATED" => created_at = parse_ics_timestamp_ms(&value),
            "COMPLETED" => {
                completed_at = parse_ics_timestamp_ms(&value);
                // Some CalDAV clients set COMPLETED without explicit STATUS:COMPLETED
                if completed_at.is_some() && status == Status::Todo {
                    status = Status::Done;
                }
            }
            "LAST-MODIFIED" => last_modified_ms = parse_ics_timestamp_ms(&value),
            "DTSTAMP" => dtstamp_ms = parse_ics_timestamp_ms(&value),
            "X-ROKE-UPDATED-MS" => roke_updated_ms = value.trim().parse::<u64>().ok(),
            "X-ROKE-ORIGIN" => {
                let slug = normalize_device_tag(&value);
                if !slug.is_empty() {
                    origin = Some(slug);
                }
            }
            "X-ROKE-ORDER" => order = value.trim().parse::<i64>().ok(),
            "X-APPLE-SORT-ORDER" => apple_order = value.trim().parse::<i64>().ok(),
            _ => {}
        }
    }

    if !saw_vtodo {
        return Err("iCalendar payload contains no VTODO component".into());
    }
    if uid.is_empty() {
        return Err("VTODO is missing required UID property".into());
    }
    if title.is_empty() {
        title = "(untitled)".into();
    }

    // Reconcile millisecond timestamp with second-resolution LAST-MODIFIED:
    // If another client (Tasks.org) edited the VTODO, LAST-MODIFIED advances past
    // X-ROKE-UPDATED-MS / 1000, so we adopt LAST-MODIFIED; otherwise preserve exact ms.
    let updated_at = match (roke_updated_ms, last_modified_ms) {
        (Some(ms), Some(lm)) if lm / 1000 > ms / 1000 => lm,
        (Some(ms), _) => ms,
        (None, Some(lm)) => lm,
        (None, None) => dtstamp_ms
            .or(completed_at)
            .or(created_at)
            .unwrap_or_else(now_millis),
    };

    if status == Status::Done && completed_at.is_none() {
        completed_at = Some(updated_at);
    } else if status != Status::Done {
        completed_at = None;
    }

    let final_origin = origin.or_else(|| {
        let slug = normalize_device_tag(fallback_origin);
        (!slug.is_empty()).then_some(slug)
    });

    Ok(Task {
        id: 0,
        uid,
        title,
        description,
        status,
        priority,
        tags,
        due,
        due_time,
        repeat,
        created_at,
        completed_at,
        updated_at,
        origin: final_origin,
        order: order.or(apple_order),
    })
}

fn owned_vtodo_lines(task: &Task, include_rrule: bool) -> Vec<String> {
    let mut out = Vec::new();
    let stamp = if task.updated_at > 0 {
        task.updated_at
    } else {
        now_millis()
    };

    out.push(format!("UID:{}", task.uid));
    out.push(format!("DTSTAMP:{}", format_utc_timestamp(stamp)));
    if let Some(created) = task.created_at {
        out.push(format!("CREATED:{}", format_utc_timestamp(created)));
    }
    out.push(format!("LAST-MODIFIED:{}", format_utc_timestamp(stamp)));
    out.push(format!("X-ROKE-UPDATED-MS:{stamp}"));
    out.push(format!("SUMMARY:{}", escape_ics_text(&task.title)));
    if !task.description.is_empty() {
        out.push(format!(
            "DESCRIPTION:{}",
            escape_ics_text(&task.description)
        ));
    }
    out.push(format!("STATUS:{}", status_to_ics(task.status)));
    out.push(format!("PRIORITY:{}", priority_to_ics(task.priority)));
    if task.status == Status::Done {
        let done_ts = task.completed_at.unwrap_or(stamp);
        out.push(format!("COMPLETED:{}", format_utc_timestamp(done_ts)));
        out.push("PERCENT-COMPLETE:100".into());
    }
    if !task.tags.is_empty() {
        let cats = task
            .tags
            .iter()
            .map(|t| escape_ics_text(t))
            .collect::<Vec<_>>()
            .join(",");
        out.push(format!("CATEGORIES:{cats}"));
    }
    if let Some(due) = &task.due {
        if let Some(line) = format_due_line(due, task.due_time.as_deref()) {
            out.push(line);
        }
    }
    if include_rrule {
        if let Some(rep) = &task.repeat {
            if let Some(rrule) = repeat_to_rrule(rep) {
                out.push(format!("RRULE:{rrule}"));
            }
        }
    }
    if let Some(ord) = task.order {
        out.push(format!("X-ROKE-ORDER:{ord}"));
    }
    if let Some(orig) = task.origin.as_deref().filter(|s| !s.is_empty()) {
        out.push(format!("X-ROKE-ORIGIN:{orig}"));
    }
    out
}

/// Serialize a `roke` [`Task`] into a standalone RFC 5545 `VCALENDAR` with one `VTODO`.
pub fn task_to_vtodo(task: &Task) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        concat!("PRODID:-//roke//roke ", env!("CARGO_PKG_VERSION"), "//EN").to_string(),
        "BEGIN:VTODO".to_string(),
    ];
    lines.extend(owned_vtodo_lines(task, true));
    lines.push("END:VTODO".to_string());
    lines.push("END:VCALENDAR".to_string());

    let mut out = String::new();
    for l in lines {
        out.push_str(&fold_line(&l));
        out.push_str("\r\n");
    }
    out
}

/// Patch an existing `VCALENDAR`/`VTODO` payload with updated fields from `task`,
/// preserving `VTIMEZONE`, nested `VALARM` blocks, geofences, subtasks (`RELATED-TO`),
/// `DTSTART`, complex `RRULE`s, and any other client-specific properties verbatim.
pub fn patch_vtodo(existing_ics: &str, task: &Task) -> String {
    let unfolded = unfold_lines(existing_ics);
    let mut result_lines: Vec<String> = Vec::new();
    let mut in_vtodo = false;
    let mut patched = false;
    let mut sub_depth: usize = 0;

    // Preserve a complex RRULE that roke doesn't understand unless the user set `task.repeat`.
    let preserve_existing_rrule = task.repeat.is_none()
        && unfolded.iter().any(|l| {
            parse_ics_line(l).is_some_and(|(n, _, v)| n == "RRULE" && rrule_to_repeat(&v).is_none())
        });

    for line in &unfolded {
        let Some((name, _, value)) = parse_ics_line(line) else {
            result_lines.push(line.clone());
            continue;
        };

        if name == "BEGIN" {
            let comp = value.trim().to_ascii_uppercase();
            if comp == "VTODO" && !in_vtodo && !patched {
                in_vtodo = true;
                result_lines.push(line.clone());
                result_lines.extend(owned_vtodo_lines(task, !preserve_existing_rrule));
                continue;
            } else if in_vtodo {
                sub_depth += 1;
                result_lines.push(line.clone());
                continue;
            }
        } else if name == "END" {
            let comp = value.trim().to_ascii_uppercase();
            if in_vtodo {
                if sub_depth > 0 {
                    sub_depth -= 1;
                    result_lines.push(line.clone());
                    continue;
                } else if comp == "VTODO" {
                    in_vtodo = false;
                    patched = true;
                    result_lines.push(line.clone());
                    continue;
                }
            }
        }

        if in_vtodo && sub_depth == 0 {
            let is_owned = matches!(
                name.as_str(),
                "UID"
                    | "DTSTAMP"
                    | "CREATED"
                    | "LAST-MODIFIED"
                    | "SUMMARY"
                    | "DESCRIPTION"
                    | "STATUS"
                    | "PRIORITY"
                    | "COMPLETED"
                    | "PERCENT-COMPLETE"
                    | "CATEGORIES"
                    | "DUE"
                    | "X-ROKE-ORDER"
                    | "X-ROKE-ORIGIN"
                    | "X-ROKE-UPDATED-MS"
            ) || (name == "RRULE" && !preserve_existing_rrule);
            if is_owned {
                continue;
            }
        }

        result_lines.push(line.clone());
    }

    if !patched {
        return task_to_vtodo(task);
    }

    let mut out = String::new();
    for l in result_lines {
        out.push_str(&fold_line(&l));
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtodo_round_trip_preserves_all_roke_fields() {
        let task = Task {
            id: 1,
            uid: "laptop-1764143800000-12-1".into(),
            title: "Buy groceries, milk; bread".into(),
            description: "- [ ] Milk\n- [x] Bread\nNotes with \\ slash".into(),
            status: Status::InProgress,
            priority: Priority::High,
            tags: vec!["errands".into(), "home".into()],
            due: Some("2026-12-01".into()),
            due_time: Some("14:30".into()),
            repeat: Some("weekly".into()),
            created_at: Some(1_764_143_800_000),
            completed_at: None,
            updated_at: 1_764_143_900_456,
            origin: Some("laptop".into()),
            order: Some(2000),
        };

        let ics = task_to_vtodo(&task);
        let parsed = vtodo_to_task(&ics, "phone").unwrap();

        assert_eq!(parsed.uid, task.uid);
        assert_eq!(parsed.title, task.title);
        assert_eq!(parsed.description, task.description);
        assert_eq!(parsed.status, Status::InProgress);
        assert_eq!(parsed.priority, Priority::High);
        assert_eq!(parsed.tags, vec!["errands", "home"]);
        assert_eq!(parsed.due, Some("2026-12-01".into()));
        assert_eq!(parsed.due_time, Some("14:30".into()));
        assert_eq!(parsed.repeat, Some("weekly".into()));
        assert_eq!(parsed.created_at, Some(1_764_143_800_000));
        assert_eq!(parsed.updated_at, 1_764_143_900_456);
        assert_eq!(parsed.origin.as_deref(), Some("laptop"));
        assert_eq!(parsed.order, Some(2000));
    }

    #[test]
    fn patch_vtodo_preserves_valarm_geofence_subtasks_and_complex_rrule() {
        let tasks_org_ics = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "PRODID:-//Tasks.org//EN\r\n",
            "BEGIN:VTODO\r\n",
            "UID:tasks-org-42\r\n",
            "DTSTAMP:20261126T100000Z\r\n",
            "LAST-MODIFIED:20261126T100000Z\r\n",
            "SUMMARY:Original phone task\r\n",
            "DESCRIPTION:Phone note\r\n",
            "STATUS:NEEDS-ACTION\r\n",
            "PRIORITY:5\r\n",
            "DTSTART;VALUE=DATE:20261125\r\n",
            "RRULE:FREQ=MONTHLY;BYMONTHDAY=-1\r\n",
            "RELATED-TO;RELTYPE=PARENT:parent-uid-99\r\n",
            "X-APPLE-STRUCTURED-LOCATION;VALUE=URI:geo:37.33,-122.03\r\n",
            "BEGIN:VALARM\r\n",
            "ACTION:DISPLAY\r\n",
            "TRIGGER:-PT15M\r\n",
            "DESCRIPTION:Alarm description must not overwrite task description\r\n",
            "END:VALARM\r\n",
            "END:VTODO\r\n",
            "END:VCALENDAR\r\n",
        );

        let mut parsed = vtodo_to_task(tasks_org_ics, "phone").unwrap();
        assert_eq!(parsed.title, "Original phone task");
        assert_eq!(parsed.description, "Phone note");
        assert_eq!(parsed.repeat, None); // complex BYMONTHDAY=-1 left untouched

        parsed.title = "Edited in roke".into();
        parsed.set_status(Status::Done);
        parsed.touch();

        let patched = patch_vtodo(tasks_org_ics, &parsed);
        assert!(patched.contains("SUMMARY:Edited in roke"));
        assert!(patched.contains("STATUS:COMPLETED"));
        assert!(patched.contains("BEGIN:VALARM"));
        assert!(patched.contains("TRIGGER:-PT15M"));
        assert!(patched.contains("RELATED-TO;RELTYPE=PARENT:parent-uid-99"));
        assert!(patched.contains("X-APPLE-STRUCTURED-LOCATION;VALUE=URI:geo:37.33,-122.03"));
        assert!(patched.contains("DTSTART;VALUE=DATE:20261125"));
        assert!(patched.contains("RRULE:FREQ=MONTHLY;BYMONTHDAY=-1"));
    }

    #[test]
    fn remote_edit_advancing_last_modified_beats_old_x_roke_updated_ms() {
        let ics = concat!(
            "BEGIN:VCALENDAR\r\n",
            "VERSION:2.0\r\n",
            "BEGIN:VTODO\r\n",
            "UID:u1\r\n",
            "SUMMARY:Updated on phone\r\n",
            "LAST-MODIFIED:20261127T120000Z\r\n",
            "X-ROKE-UPDATED-MS:1700000000123\r\n",
            "END:VTODO\r\n",
            "END:VCALENDAR\r\n",
        );
        let parsed = vtodo_to_task(ics, "phone").unwrap();
        let lm = parse_ics_timestamp_ms("20261127T120000Z").unwrap();
        assert_eq!(parsed.updated_at, lm);
    }

    #[test]
    fn rrule_round_trips_all_roke_recurrence_rules() {
        for rule in [
            "daily", "weekdays", "weekly", "monthly", "every 3d", "every 2w", "every 6m",
        ] {
            let rrule = repeat_to_rrule(rule).expect(rule);
            assert_eq!(rrule_to_repeat(&rrule).as_deref(), Some(rule));
        }
        assert_eq!(rrule_to_repeat("FREQ=MONTHLY;BYMONTHDAY=-1"), None);
    }
}
