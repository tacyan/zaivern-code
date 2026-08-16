# Zaivern Code — Language Packs

Flat JSON, one file per language, **stable IDs as keys**.

```json
{
  "app.new_session": "New Session",
  "app.settings": "Settings",
  "agent.send": "Send",
  "agent.stop": "Stop",
  "agent.approve": "Approve",
  "agent.approve_all": "Approve All",
  "agent.broadcast": "Broadcast to All Agents"
}
```

| File | Language | |
|---|---|---|
| `en.json` | English | **base** — every other language falls back to this |
| `ja.json` | 日本語 | **source** — the text the code itself is written in |
| `zh-CN.json` | 简体中文 | |
| `ko.json` | 한국어 | |
| `pt-BR.json` | Português (Brasil) | |
| `es.json` | Español | |

These six are compiled into the binary (`include_str!`), so every install has
every language with no extra files.

## Add a language — no rebuild needed

```sh
zai lang export fr                 # writes ~/.zaivern/locales/fr.json
$EDITOR ~/.zaivern/locales/fr.json
zai lang check fr                  # exits 1 if anything does not line up
zai lang set fr
```

Drop a file in `~/.zaivern/locales/` (or `~/.config/zaivern/locales/`) and it
shows up in the 🌐 picker. Using an **ID that already exists overrides the
built-in translation** — a one-line file is enough to fix one string.

## Share it

```sh
zai lang list --remote                                  # what the source repo has
zai lang install zh-CN
zai lang install fr --from someone/zaivern-lang-fr      # any GitHub repo with locales/
```

Put your `locales/<id>.json` in a public GitHub repo and anyone can install it
with `--from owner/repo`. Nothing is hard-coded: the default source is read from
this project's own installer, and `ZAIVERN_LANG_REPO` overrides it.

`zai lang install` **validates before it writes** (fail-closed):

- must parse as a JSON object of strings
- must not be empty
- **`{placeholder}` names must match `en.json`** — a translation that drops
  `{path}` would leave a hole at runtime
- never silently overwrites an existing file (`--force` to replace)

Missing keys are fine: they fall back to English, so a partial pack is usable
from day one. The installer tells you how many are missing.

## Rules for translators

1. **Keep `{name}` placeholders exactly.** Same set, same spelling, in every
   language. Word order may change; the placeholders may not.
2. **Keep leading/trailing emoji, symbols, spaces and newlines.**
   `"📣 全エージェントへブロードキャスト"` → `"📣 Broadcast to All Agents"`.
3. **Do not translate key names, commands, paths, flags or product names**
   (`git`, `worktree`, `LSP`, `PTY`, `Zaivern`, `Claude Code`, `Codex`, `MCP`,
   `Tailscale`, `⌘⇧C`, `config.toml`, …).
4. **Buttons and menu items stay short.** Add a full stop only where the
   original has one.
5. `_`-prefixed keys are ignored by the loader — use them for notes to other
   translators.

## For maintainers

```sh
zai lang missing                   # strings on screen that are not in the dictionary
zai lang missing src /tmp/d.json   # same, as a shard to hand to a translator
zai lang apply /tmp/d.json         # merge translations back into all six files
zai lang check ja
```

The guards live in `src/locale.rs` and `src/tutorial.rs` tests. If one goes red,
**fix the JSON, not the test**. Details: [`docs/i18n.md`](../docs/i18n.md).
