-- Durable project-local observations. Never extrapolate an open endpoint.
CREATE TABLE IF NOT EXISTS operating_intervals (
    interval_id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    control_epoch INTEGER NOT NULL,
    start_unix_ms INTEGER NOT NULL,
    end_unix_ms INTEGER NOT NULL CHECK(end_unix_ms >= start_unix_ms),
    close_reason TEXT NOT NULL CHECK(close_reason IN ('open','paused','restart','gap','control_changed'))
) STRICT;
CREATE INDEX IF NOT EXISTS operating_intervals_window ON operating_intervals(start_unix_ms,end_unix_ms);
CREATE UNIQUE INDEX IF NOT EXISTS operating_intervals_open ON operating_intervals(close_reason) WHERE close_reason='open';
CREATE TABLE IF NOT EXISTS operating_clock (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    first_unix_ms INTEGER NOT NULL,
    last_unix_ms INTEGER NOT NULL,
    session TEXT NOT NULL,
    active INTEGER NOT NULL CHECK(active IN (0,1)),
    control_epoch INTEGER NOT NULL,
    cadence_ms INTEGER NOT NULL CHECK(cadence_ms > 0)
) STRICT;
CREATE TABLE IF NOT EXISTS operating_gaps (
    gap_id INTEGER PRIMARY KEY,
    start_unix_ms INTEGER NOT NULL,
    end_unix_ms INTEGER NOT NULL CHECK(end_unix_ms >= start_unix_ms),
    reason TEXT NOT NULL CHECK(reason IN ('restart','gap','control_changed'))
) STRICT;
