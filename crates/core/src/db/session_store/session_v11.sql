-- Ownership keys use lowercase compact UUIDs, retaining the notice namespace.
-- Keep transcript IDs verbatim for sync references, tools and turn links.
DROP TRIGGER IF EXISTS remember_removed_message;
ALTER TABLE removed_message_owners RENAME TO old_removed_message_owners;
CREATE TABLE removed_message_owners (
    id TEXT NOT NULL, role TEXT NOT NULL, PRIMARY KEY (id, role)
);
-- Version 9 recorded hyphenated IDs. Its GLOB also allowed non-hex characters.
INSERT OR IGNORE INTO removed_message_owners
SELECT prefix || hex_id,role FROM (
    SELECT CASE WHEN substr(id,1,7)='notice-' THEN 'notice-' ELSE '' END AS prefix,
           lower(replace(CASE WHEN substr(id,1,7)='notice-' THEN substr(id,8) ELSE id END,'-','')) AS hex_id, role
    FROM old_removed_message_owners
) WHERE length(hex_id)=32 AND hex_id NOT GLOB '*[^0-9a-f]*'
    AND substr(hex_id,1,2)='dc' AND substr(hex_id,13,1)='8';
DROP TABLE old_removed_message_owners;

CREATE TRIGGER remember_removed_message AFTER DELETE ON messages BEGIN
    INSERT OR IGNORE INTO removed_message_owners
    WITH parsed(prefix, raw) AS (
        SELECT CASE WHEN substr(OLD.id,1,7)='notice-' THEN 'notice-' ELSE '' END,
               CASE WHEN substr(OLD.id,1,7)='notice-' THEN substr(OLD.id,8) ELSE OLD.id END
    ), unwrapped(prefix, uuid) AS (
        SELECT prefix, CASE
            WHEN length(raw)=38 AND substr(raw,1,1)='{' AND substr(raw,-1)='}' THEN substr(raw,2,36)
            WHEN length(raw)=45 AND substr(raw,1,9)='urn:uuid:' THEN substr(raw,10)
            ELSE raw END
        FROM parsed
    ), compact(prefix, hex_id) AS (
        SELECT prefix, lower(replace(uuid,'-','')) FROM unwrapped
        WHERE length(uuid)=32 OR uuid GLOB '????????-????-????-????-????????????'
    )
    SELECT prefix || hex_id, OLD.role FROM compact
    WHERE length(hex_id)=32 AND hex_id NOT GLOB '*[^0-9a-f]*'
        AND substr(hex_id,1,2)='dc' AND substr(hex_id,13,1)='8';
END;
