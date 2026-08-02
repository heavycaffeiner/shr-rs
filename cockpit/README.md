# SHR-RS Cockpit dashboard

Cockpit frontend over the schema-versioned output of:

```sh
shr-rs status --json
```

The page shows physical disks, SMART health, mdadm arrays, member counts and
sync/rebuild progress, and drives the write paths — group creation, expand,
scrub, replace, recompress, snapshot, schedule, destroy — through the same
engine commands. Every write goes through Cockpit's `superuser: "require"`,
and each destructive one requires its preview first. Nothing is estimated: the
page leaves usable capacity and assigned partition bytes blank rather than
guessing when the status schema does not report them.

## Languages

The page follows the session language Cockpit is set to: English and Korean
ship today, and anything else falls back to English.

Source strings are stable dotted keys (`panels.drives.col.node`), not English
sentences, so **English is a translation like any other** and lives in
`po/en.po` next to `po/ko.po`. Adding a string means adding the key to both
files; adding a language means one more `po/<lang>.po`.

That choice needs one thing Cockpit does not do by itself. cockpit-ws answers
a request for `po.js` with `po.<lang>.js` when it has one and with an *empty*
body otherwise — English included, because it assumes English is the msgid.
So `build.js` also emits `po-default.js` (the English catalogue under a name
carrying no language segment, hence served verbatim to every session), and
`index.html` loads it before `po.js`. `cockpit.locale()` merges, so a Korean
session applies English first and Korean on top — which is also what makes a
partial translation fall back per string instead of showing raw keys.

`npm test` loads `po/en.po` (see `src/testCatalog.ts`) so assertions read as
English text rather than keys, and a key missing from `po/en.po` fails a test.

```sh
make po/shr-rs.pot                     # re-extract keys from src/
msgmerge -U po/ko.po po/shr-rs.pot     # fold them into a catalogue
```

## Requirements

- Cockpit 356 or newer
- `shr-rs` available in `PATH`
- Node.js 22.18 or newer for development and tests

## Develop and verify

```sh
npm install
npm test
npm run typecheck
npm run eslint
npm run build
```

Install the built page for the current user:

```sh
mkdir -p ~/.local/share/cockpit
ln -s "$PWD/dist" ~/.local/share/cockpit/shr-rs
```

`npm run build` also writes `dist/THIRD-PARTY-NOTICES.txt`, listing every
module esbuild pulled into the bundle plus the webfonts emitted beside it. It
is generated from the bundler's own metafile, so it cannot drift from what is
shipped; packages that carry no license text of their own are supplied from
`notices/overrides.json`, and a bundled module with neither fails the build.

Then open `/shr-rs` in Cockpit. The browser integration test in
`test/check-application` verifies the live `status --json` schema and the
rendered dashboard; it runs through Cockpit's FMF gating harness
(`plans/all.fmf`), not through `npm test`.
