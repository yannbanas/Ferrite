WITH recent (id) AS (SELECT id FROM events WHERE at > 0)
SELECT u.id, count(*) AS n
FROM users u
LEFT JOIN recent r ON r.id = u.id
WHERE u.email LIKE '%@example.com' AND u.age BETWEEN 18 AND 99
GROUP BY u.id
HAVING count(*) > 1
ORDER BY n DESC NULLS LAST
LIMIT 10 OFFSET 5;
CREATE TABLE public.users (id UUID PRIMARY KEY, email TEXT NOT NULL, profile JSONB);
INSERT INTO users (id, email) VALUES ($1, 'a@b.c') RETURNING id;
UPDATE users SET email = lower(email) WHERE id IN (SELECT id FROM recent);
DELETE FROM users WHERE id IS NULL;
CREATE PROCEDURE guard(caller UUID) AS BEGIN
  IF caller <> CAST('0' AS UUID) THEN RAISE 'denied'; ELSE RETURN 1; END IF;
END;
CREATE TRIGGER audit AFTER INSERT OR UPDATE ON users FOR EACH ROW EXECUTE PROCEDURE guard($1);
BEGIN; COMMIT; ROLLBACK;
