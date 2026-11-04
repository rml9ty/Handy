# rml9ty/Handy fork

Official Handy (cjpais/Handy) plus game chat options (originally by babyjay3000,
branch `feature/game-chat-options` of babyjay3000/Handy).

The fork is kept as a small stack of commits on top of official `main`:

1. `feat: add game chat options for voice-to-chat in games` (the features)
2. `chore(fork): point updater at this fork and add Windows release workflow`
   - `src-tauri/tauri.conf.json`: updater `endpoints` -> this repo's releases,
     updater `pubkey` -> this fork's key, Windows `signCommand` removed
   - `.github/workflows/fork-release.yml`
3. this file
4. `feat: remove a shortcut with the X button` (empty binding = shortcut off;
   register/unregister skip it in `tauri_impl.rs`, `handy_keys.rs`,
   `secure_input.rs`; `change_binding` accepts ""; UI in `ShortcutInput.tsx`)
5. `ci(fork): drop macOS from Main Branch Build` (no Apple signing secrets
   here, so those jobs always failed; `.github/workflows/main-build.yml`)

## Updating to a new official Handy version

1. On GitHub: **Sync fork -> Update branch**. If that works, go to step 3.
2. If GitHub reports conflicts, resolve them locally:
   ```
   git remote add upstream https://github.com/cjpais/Handy.git   # once
   git fetch upstream
   git merge upstream/main     # fix conflicts, keep upstream logic + fork bits
   git push
   ```
   Conflicts seen so far: `src-tauri/src/actions.rs` (the paste call must stay
   `utils::paste_with_mode(final_text, ah_clone.clone(), paste_mode)`) and
   `src/bindings.ts` (generated; keep upstream's version and re-add the
   `open_input_field*`, `capitalization`, `slash_command_digits` fields and the
   `OpenInputFieldKey` / `CapitalizationMode` types).
   Check that `tauri.conf.json` still has this fork's updater endpoint, pubkey
   and no `signCommand`.
3. Actions -> **Fork Release (Windows)** -> Run workflow. The version comes from
   upstream's `tauri.conf.json`, so the app sees it as an update.
   For a fork-only fix without a new official version, raise the patch version
   in `tauri.conf.json` first (e.g. `0.10.0` -> `0.10.1`); the updater ignores
   releases that aren't a higher version. The workflow refuses nothing, but a
   second run with the same version fails on the existing tag.

Repository secret needed: `TAURI_SIGNING_PRIVATE_KEY` (updater signing key).
Losing it means installed copies can't verify updates and need one manual
reinstall with a new key.
