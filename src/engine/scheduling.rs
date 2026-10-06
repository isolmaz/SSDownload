//! Queue windows, quota periods and per-job scheduling state.

use super::*;

pub(super) fn window_weekday_matches(window: &QueueWindow, weekday: u8) -> bool {
    window.weekdays.is_empty() || window.weekdays.contains(&weekday)
}

pub(super) fn queue_window_occurrence<Tz: TimeZone>(
    window: &QueueWindow,
    now: &DateTime<Tz>,
) -> Option<chrono::NaiveDate> {
    let start = parse_minute(&window.start)?;
    let end = parse_minute(&window.end)?;
    let minute = now.hour() * 60 + now.minute();
    let today = now.date_naive();
    let today_weekday = now.weekday().num_days_from_monday() as u8;
    let yesterday = today.pred_opt()?;
    let yesterday_weekday = (today_weekday + 6) % 7;
    if start == end {
        let (date, weekday) = if minute >= start {
            (today, today_weekday)
        } else {
            (yesterday, yesterday_weekday)
        };
        return window_weekday_matches(window, weekday).then_some(date);
    }
    if start < end {
        return (minute >= start && minute < end && window_weekday_matches(window, today_weekday))
            .then_some(today);
    }
    if minute >= start && window_weekday_matches(window, today_weekday) {
        Some(today)
    } else if minute < end && window_weekday_matches(window, yesterday_weekday) {
        Some(yesterday)
    } else {
        None
    }
}

pub(super) fn queue_window_period_key(window: &QueueWindow, date: chrono::NaiveDate) -> String {
    let mut weekdays = String::with_capacity(7);
    if window.weekdays.is_empty() {
        weekdays.push('*');
    } else {
        for weekday in 0..=6 {
            if window.weekdays.contains(&weekday) {
                weekdays.push(char::from(b'0' + weekday));
            }
        }
    }
    format!("window:{date}:{}:{}:{weekdays}", window.start, window.end)
}

pub(super) fn legacy_queue_window_period_key(
    index: usize,
    window: &QueueWindow,
    date: chrono::NaiveDate,
) -> String {
    format!("window:{index}:{date}:{}", window.start)
}

pub(super) fn queue_period_key<Tz: TimeZone>(
    queue: &QueuePolicy,
    now: &DateTime<Tz>,
) -> Option<String> {
    if queue.windows.is_empty() {
        return Some(format!("day:{}", now.date_naive()));
    }
    queue
        .windows
        .iter()
        .filter_map(|window| {
            queue_window_occurrence(window, now).map(|date| queue_window_period_key(window, date))
        })
        .min()
}

pub(super) fn queue_period_key_matches<Tz: TimeZone>(
    queue: &QueuePolicy,
    saved_key: &str,
    current_key: &str,
    now: &DateTime<Tz>,
) -> bool {
    if saved_key == current_key {
        return true;
    }
    queue.windows.iter().enumerate().any(|(index, window)| {
        queue_window_occurrence(window, now).is_some_and(|date| {
            saved_key == legacy_queue_window_period_key(index, window, date)
                && current_key == queue_window_period_key(window, date)
        })
    })
}

pub(super) fn refresh_queue_quota_periods_at<Tz: TimeZone>(
    settings: &mut Settings,
    now: &DateTime<Tz>,
) -> bool {
    let mut changed = false;
    for queue in &mut settings.queues {
        let Some(period_key) = queue_period_key(queue, now) else {
            continue;
        };
        let current = queue.quota.as_ref().is_some_and(|quota| {
            queue_period_key_matches(queue, &quota.period_key, &period_key, now)
        });
        if let Some(quota) = &mut queue.quota {
            if !current {
                quota.consumed_bytes = 0;
            }
            if quota.period_key != period_key {
                quota.period_key = period_key;
                changed = true;
            }
        }
    }
    changed
}

pub(super) fn queue_accepts_work<Tz: TimeZone>(
    settings: &Settings,
    queue_id: &str,
    now: &DateTime<Tz>,
) -> bool {
    let Some(queue) = settings.queues.iter().find(|queue| queue.id == queue_id) else {
        return false;
    };
    if !queue.enabled {
        return false;
    }
    let Some(period_key) = queue_period_key(queue, now) else {
        return false;
    };
    queue.quota.as_ref().is_none_or(|quota| {
        !queue_period_key_matches(queue, &quota.period_key, &period_key, now)
            || quota.consumed_bytes < quota.limit_bytes
    })
}

pub(super) fn job_scheduling_state_at<Tz: TimeZone>(
    settings: &Settings,
    request: &AddRequest,
    now: &DateTime<Tz>,
) -> JobState {
    if request.start_at.is_some_and(|at| at > now.timestamp())
        || !daily_schedule_open_at(settings, now)
        || !queue_accepts_work(
            settings,
            request.queue_id.as_deref().unwrap_or(DEFAULT_QUEUE_ID),
            now,
        )
    {
        JobState::Scheduled
    } else {
        JobState::Queued
    }
}

pub(super) fn reconcile_job_scheduling_state_at<Tz: TimeZone>(
    settings: &Settings,
    job: &mut Job,
    now: &DateTime<Tz>,
) -> bool {
    if !matches!(job.state, JobState::Queued | JobState::Scheduled) {
        return false;
    }
    let state = job_scheduling_state_at(settings, &job.request, now);
    if job.state == state {
        return false;
    }
    job.state = state;
    job.speed = 0;
    job.eta = None;
    job.phase = state.label().into();
    job.updated_at = now.timestamp();
    true
}

pub(super) fn consume_queue_bytes(settings: &mut Settings, queue_id: &str, bytes: u64) {
    let now = Local::now();
    consume_queue_bytes_at(settings, queue_id, bytes, &now);
}

pub(super) fn consume_queue_bytes_at<Tz: TimeZone>(
    settings: &mut Settings,
    queue_id: &str,
    bytes: u64,
    now: &DateTime<Tz>,
) {
    if bytes == 0 {
        return;
    }
    let Some(queue) = settings
        .queues
        .iter_mut()
        .find(|queue| queue.id == queue_id)
    else {
        return;
    };
    let period_key = queue_period_key(queue, now);
    let current = period_key.as_ref().is_some_and(|period_key| {
        queue.quota.as_ref().is_some_and(|quota| {
            queue_period_key_matches(queue, &quota.period_key, period_key, now)
        })
    });
    let Some(quota) = &mut queue.quota else {
        return;
    };
    if let Some(period_key) = period_key {
        if !current {
            quota.consumed_bytes = 0;
        }
        if quota.period_key != period_key {
            quota.period_key = period_key;
        }
    }
    quota.consumed_bytes = quota.consumed_bytes.saturating_add(bytes);
}
