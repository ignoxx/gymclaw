-- Preferred weight display and input unit; stored weights remain canonical kg.
ALTER TABLE user_profile ADD COLUMN weight_unit VARCHAR NOT NULL DEFAULT 'kg' CHECK (weight_unit IN ('kg', 'lb'));
