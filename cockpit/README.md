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
