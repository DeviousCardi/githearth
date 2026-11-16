# Abuse contact and takedown runbook

This is the public-host abuse baseline for M4.2. It is a prerequisite for
M11a's self-service sign-up and deletion; the endpoint that creates accounts is
not enabled before then.

## Contact

Publish one address in configuration and on the public site:

```toml
abuse_contact = "abuse@example.com"
```

The address must be a real mailbox a person reads, not a black hole. `doctor`'s
**email DNS** check fails when mail is configured and this is empty. Put the
same address in the site's security contact and in the DNS
`security.txt`/WHOIS records if the domain publishes them.

## What counts as abuse

- Spam, phishing, malware or illegal content in a repository, issue, comment,
  pull request, release or Git LFS/package object.
- Automated scanning, credential stuffing or a request flood that consumes
  another organization's share.
- A repository or account impersonating a person or organization.
- Unsolicited bulk email sent through the installation's relay.

## Takedown

1. **Record the report.** Create a private operator note with the date, the
   reporter (if they consent to being named), the URL or repository, the
   category and the evidence. Do not copy credentials, tokens or private
   repository contents into the note.
2. **Preserve, then act.** Take a backup or record the audit range before a
   destructive change when the report may become a legal matter. The audit
   log is append-only; use it rather than editing rows.
3. **Disable the smallest thing that stops the abuse.**
   - A repository: archive it (writes refused, reads remain) or delete it into
     trash under the retention window. A deleted repository returns `410` and
     never redirects ([repositories](repositories.md)).
   - An account: disable it (sessions, tokens and SSH keys stop admitting) or
     tombstone it once M11a ships deletion. Disabling the last owner of an
     organization or the last site administrator is refused; transfer
     ownership first.
   - An organization: suspend it; every member's requests are refused while
     the row is suspended.
   - A request flood: the per-address, per-organization and global limits in
     [limits](limits.md) are the first control; a firewall rule at the edge is
     the second.
4. **Notify.** Tell the reporter the outcome and the reason at the level you
   can disclose. If a legal request requires silence, record that decision in
   the operator note.
5. **Preserve evidence for the retention period.** The audit chain and the
   outbox are the record; a backup is encrypted and key-owned by the operator
   ([backup-restore](backup-restore.md)). Do not retain a copy of the abusive
   content longer than the policy requires.
6. **Review.** If the abuse used a limit, record which budget it exhausted and
   tune the published defaults rather than removing the limit. If it used a
   missing control, open a decision record before changing the baseline.

## Sign-up and recovery

Self-service sign-up stays **off** until M11a ships account and organization
deletion, email verification and the abuse controls above. The M4.2 limiter
(`signup_velocity_per_address`, default 3 per address per hour) is the one
M11a's endpoint calls; it is tested at its interface now. A host that enables
sign-up must also have SPF, DKIM, DMARC and a bounce address configured; see
[email](email.md).
