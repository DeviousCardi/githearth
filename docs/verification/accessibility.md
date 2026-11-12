# Accessibility review (WCAG 2.2 AA basics)

Date: 2026-11-11. Scope: every server-rendered HTML page — the shared shell in
`ui.rs` and the page handlers in `browse_ui.rs`, `issues_ui.rs`,
`notifications_ui.rs`, `pulls_ui.rs` and `settings_ui.rs`. All pages render
through one `render_page` shell, so the shell-level checks below apply to every
page; the per-form checks were read against each form handler.

## Checklist and result

| Item | Result |
| --- | --- |
| `lang` attribute | **Pass.** `render_page` emits `<html lang="en">` on every page. |
| Labels for every input | **Pass.** Every visible input, textarea and select has an associated `<label for="…">` (login, new/edit issue, new pull, comment, review, merge, token and SSH-key forms). Checkbox groups are wrapped in a `<label>` and grouped with a `<fieldset><legend>`. Hidden CSRF/idempotency inputs have no visible label by design. |
| Landmarks | **Pass.** Every page has `<nav aria-label="Primary">` (the top navigation), `<nav aria-label="Breadcrumb">` on drill-down pages, and `<main id="main">`. Tables add a `<caption>`. |
| Heading order | **Pass.** Every page starts at `<h1>` and, where sections follow, uses `<h2>` in order (`Settings`, the pull page's Reviews/Comments, the issue page's Comments, the compare page's Files changed). No level is skipped. |
| Focus visibility | **Fixed.** A `:focus-visible` outline (`3px solid #005fcc`, 2px offset) was added for links, buttons, inputs, textareas and selects. |
| Colour contrast of the CSS | **Pass.** Measured ratios: body text `#111` on `#f7f7f8` 17.6:1; nav `#fff` on `#111` 18.9:1; nav muted `#ccc` on `#111` 11.8:1; `.muted` `#666` on white 5.7:1; `.error` `#b00020` on white 7.3:1. All exceed AA (4.5:1 text, 3:1 non-text); the focus outline is ≥ 3:1 against both the light page and the dark nav (3.155:1 on `#111`). |
| Alt text | **Pass / not applicable.** No `<img>` elements are rendered. |
| Form error messages tied to fields | **Pass.** Validation errors are shown as text with `role="alert"`. The sign-in form additionally ties its credential error to the username and password fields with `aria-invalid="true"` and `aria-describedby="login-error"`. |
| Skip link | **Fixed.** A "Skip to content" link (`<a class="skip-link" href="#main">`) was added as the first focusable element; it is visually hidden until focused and targets `<main id="main" tabindex="-1">`. |

## Changes made

- `ui.rs`: added the skip link, `id="main" tabindex="-1"`, an `aria-label` on the
  primary navigation, a `:focus-visible` outline and a visible-on-focus skip
  link style; tied the sign-in credential error to the username/password fields.
- `tests/ui_login.rs` and `ui.rs` unit tests: assert the `lang` attribute, the
  skip link, the labelled `main`/`nav` landmarks, the focus-visible styles, and
  the field-tied login error.

## Known limits

Colour contrast was computed from the CSS constants, not measured on a rendered
screen; it is monotonic and comfortably above the thresholds. The review covers
structure, labels, headings, focus and contrast — the WCAG basics listed in the
M4 exit scope — not a full automated axe/WAVE pass or a screen-reader walkthrough,
which remain for a later milestone.
