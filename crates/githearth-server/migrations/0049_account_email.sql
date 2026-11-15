-- Account email addresses (M4.1 acceptance case U06).
--
-- An invitation is addressed to an email address, and only the account that
-- owns that address may accept it (ADR 0015: "the only destination email a
-- notification or a recovery link may use" is the verified one). M4.1's account
-- administration (0041, 0044) never added the column, so this migration adds
-- the primary address, its verification time and a case-insensitive uniqueness
-- constraint.
--
-- Two accounts cannot share an address, whatever the case of its letters: a
-- duplicate address would make "the account this invitation is for" ambiguous.
-- The verification time starts NULL: nothing marks an address verified until a
-- verification flow sets it, and accepting an invitation requires it.
--
-- It is kept separate from 0048 so that two slices allocate one number each.

ALTER TABLE accounts ADD COLUMN email text NULL;
ALTER TABLE accounts ADD COLUMN email_verified_at timestamptz NULL;
-- The application's `valid_email` rule, so a SQL insert cannot store an
-- address the product would refuse: lower case, one `@` exactly with a local
-- part of 1–64 characters that does not begin with a dot, a domain of at least
-- one dot with no leading or trailing dot and no empty label, and every
-- character inside ASCII 32–126, which excludes control characters and
-- non-ASCII bytes. (The space is inside that range, as it is in the application
-- validator too: the domain/dot rules, not a space ban, keep a space out of a
-- real address, and accepting it here keeps the CHECK and the validator equal.)
ALTER TABLE accounts ADD CONSTRAINT accounts_email_format CHECK (
  email IS NULL OR (
    length(email) BETWEEN 3 AND 320
    AND email = lower(email)
    AND email ~ '^[ -~]+$'
    AND length(email) - length(replace(email, '@', '')) = 1
    AND strpos(email, '@') > 1
    AND length(split_part(email, '@', 1)) BETWEEN 1 AND 64
    AND left(split_part(email, '@', 1), 1) <> '.'
    AND split_part(email, '@', 2) <> ''
    AND strpos(split_part(email, '@', 2), '.') > 1
    AND right(split_part(email, '@', 2), 1) <> '.'
    AND split_part(email, '@', 2) NOT LIKE '%..%'
  )
);
-- The same rule with its NULL guard removed, on a table of its own, so the
-- suite can run one table of cases against both and prove they agree. It is
-- created empty here and every statement writes no rows; the test drops it.
CREATE TABLE accounts_email_format_check (
  email text NOT NULL,
  CONSTRAINT email_format CHECK (
    length(email) BETWEEN 3 AND 320
    AND email = lower(email)
    AND email ~ '^[ -~]+$'
    AND length(email) - length(replace(email, '@', '')) = 1
    AND strpos(email, '@') > 1
    AND length(split_part(email, '@', 1)) BETWEEN 1 AND 64
    AND left(split_part(email, '@', 1), 1) <> '.'
    AND split_part(email, '@', 2) <> ''
    AND strpos(split_part(email, '@', 2), '.') > 1
    AND right(split_part(email, '@', 2), 1) <> '.'
    AND split_part(email, '@', 2) NOT LIKE '%..%'
  )
);
CREATE UNIQUE INDEX accounts_email_unique ON accounts (lower(email)) WHERE email IS NOT NULL;
