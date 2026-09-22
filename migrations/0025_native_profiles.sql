-- Only sealed native verification results enter through the service API.
-- Reports remain evidence, not launch or owner authority.
CREATE TABLE native_profiles (
    profile_digest TEXT PRIMARY KEY CHECK(length(profile_digest)=64),
    report TEXT NOT NULL CHECK(json_valid(report) AND length(report)<=1048576),
    report_digest TEXT NOT NULL CHECK(length(report_digest)=64),
    sequence INTEGER NOT NULL UNIQUE REFERENCES events(sequence)
) STRICT;
CREATE TRIGGER native_profiles_no_update BEFORE UPDATE ON native_profiles
BEGIN SELECT RAISE(ABORT,'native profile is immutable'); END;
CREATE TRIGGER native_profiles_no_delete BEFORE DELETE ON native_profiles
BEGIN SELECT RAISE(ABORT,'native profile is immutable'); END;
UPDATE store_meta SET schema_version=25;
PRAGMA user_version=25;
