# Copilot Prompt: Safely Publish Subtitler Foundation

Use this prompt in the existing **Subtitler** VS Code workspace. It is written
for the current Windows development environment and must preserve all local
work.

```text
You are preparing the current Subtitler repository for a safe first push.

Repository: S:\Documents\GitHub\Subtitler
Target GitHub repository: https://github.com/dragonscypher/Subtitler.git
Current local state: branch `main` has no commits; project files are untracked.
The remote currently has a `main` branch. Do not overwrite it and never use
`git reset --hard`, `git checkout --`, `git clean`, or `git push --force`.

Environment verified:
- Windows AMD64
- VS Code 1.121.0, Copilot extension/status available
- Node 24.16.0, npm 11.16.0
- Rust 1.97.1, Git 2.39.1.windows.1
- Native host build: native/target/release/subtitler-native-host.exe
- Extension build: extension/dist/

Required outcome:
1. Preserve every local source file.
2. Fetch and inspect `origin/main` before choosing an integration path.
3. Create a non-force-pushed review branch named
   `codex/subtitler-foundation` based on `origin/main` when checkout can occur
   without replacing an untracked file. If a tracked remote file conflicts
   with an untracked local file, stop and explain the exact conflict rather
   than deleting or overwriting anything.
4. Stage only intended source and documentation paths:
   `.github`, `.gitignore`, `README.md`, `docs`, `extension`, `native`, and
   `scripts`. Respect `.gitignore`; never stage `extension/node_modules`,
   `extension/dist`, `native/target`, `.env*`, model files, caches, exports,
   private media, or `artifacts/` demo recordings.
5. Run these checks before commit:
   - `npm --prefix extension run validate`
   - `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\verify.ps1`
6. Review `git diff --cached --check` and `git status --short` for secrets,
   generated output, media, or unexpected files.
7. Commit with:
   `feat: add local-first Subtitler foundation`
8. Push only that review branch with:
   `git push -u origin codex/subtitler-foundation`
9. Return the exact branch URL and a concise change/test summary. Do not merge
   or open a pull request unless explicitly requested.

If GitHub authentication is needed, stop at the authentication screen and ask
the developer to complete it manually. Do not request, print, store, or copy
tokens, passwords, browser cookies, API keys, recording URLs, or transcript
contents.
```

The local browser-demo recording belongs under `artifacts/` and is intentionally
ignored by Git unless the owner later makes a separate, explicit decision to
publish it.
