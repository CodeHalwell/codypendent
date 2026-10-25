-- NULL preserves unknown completeness for historical measurements.
ALTER TABLE runs ADD COLUMN usage_coverage TEXT;
