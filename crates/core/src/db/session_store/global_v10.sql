ALTER TABLE im_user_permissions RENAME TO im_user_permissions_v9;
CREATE TABLE im_user_permissions (
    id TEXT PRIMARY KEY,
    platform TEXT NOT NULL,
    user_id TEXT NOT NULL,
    permission_mode TEXT NOT NULL DEFAULT 'allow' CHECK(permission_mode IN ('allow', 'deny', 'open', 'whitelist', 'pairing')),
    is_allowed INTEGER DEFAULT 1,
    paired_code TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
INSERT INTO im_user_permissions (id, platform, user_id, permission_mode, is_allowed, paired_code, created_at, updated_at)
SELECT id, platform, user_id, permission_mode, is_allowed, paired_code, created_at, updated_at FROM im_user_permissions_v9;
DROP TABLE im_user_permissions_v9;
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_platform_user ON im_user_permissions(platform, user_id);
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_user_id ON im_user_permissions(user_id);
CREATE INDEX IF NOT EXISTS idx_im_user_permissions_paired_code ON im_user_permissions(paired_code);
