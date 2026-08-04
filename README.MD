# Sepia - A macOS File Browser That Sucks Less

Finder is really not great, so here is a Nautilus inspired file browser for
macOS that doesn't try to be slick, but still be functional for day to day uses.

Lots of things are broken, or are wonky at the least, but I use it day to day now
and felt it was good enough to commit.

## Building

Quick dev iteration (not a real macOS app bundle — no icon, generic
app-menu name, and things like QuickLook thumbnail generation run slow
since the binary is unsigned and bundle-less):

```
cargo build
```

Runs the result at `target/debug/sepia`.

To build and package as a real `.app` bundle instead:

```
scripts/bundle.sh [release] [--install]
```

- `release` — builds with `cargo build --release` instead of the debug
  profile.
- `--install` — after building, copies the bundle to
  `/Applications/Sepia.app` (replacing whatever's there).

This assembles `target/Sepia.app`: a proper `Info.plist`, an app icon
rasterized from `app/icon.svg`, and a code signature (prefers a real
"Apple Development" identity if one's in the keychain — free via Xcode +
an Apple ID, no paid membership needed — falling back to ad-hoc signing
otherwise).

Requires `rsvg-convert` (`brew install librsvg`) plus `iconutil` and
`codesign`, both already part of macOS.

## License

GPL-2.0-only — see [LICENSE](LICENSE).
