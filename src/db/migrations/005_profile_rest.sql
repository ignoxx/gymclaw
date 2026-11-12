-- One rest time for every exercise (since 4a4b38c). The compound rest default, warmup policy and
-- recovery weight were stored but never read.
ALTER TABLE user_profile DROP COLUMN default_compound_rest_seconds;
ALTER TABLE user_profile DROP COLUMN warmup_policy;
ALTER TABLE user_profile DROP COLUMN recovery_weight;
ALTER TABLE user_profile RENAME COLUMN default_accessory_rest_seconds TO rest_seconds;
