# wiki-links spore

Obsidian-style knowledge, with or without a vault.

- `.aneural/notes/*.md` become `Note` nodes attached to their file.
- Every `[[Wiki Link]]` in any markdown file becomes a `RELATES_TO` edge.
- Links resolve by **name**, not by the path the link carries. Obsidian writes
  `[[../../Notes|Notes]]` when a link crosses folders; that path goes stale as soon as a note
  moves, while the name is what the author typed. A trailing backslash is markdown escaping, not
  part of the name: inside a table the alias separator must be written `\|` or the row breaks, and
  that is how most cross-folder links in a real vault are written. A note also answers to any
  `aliases:` it declares in frontmatter, and a real file of a given name always beats another
  note's alias.
- A link to a note nobody has written yet becomes a `MissingNote` rather than being dropped —
  the way Obsidian keeps an unresolved link in its own graph. In a real vault a good fraction of
  links are unresolved, and which notes are being asked for, by how many callers, is worth more
  than nothing. One placeholder is shared by every file that asks for it and disappears when the
  last link to it goes.

- A frontmatter `tags:` list becomes one `Tag` node per distinct tag, joined to every file
  carrying it. One node per name, not one per file, because that is what makes "everything tagged
  `fda`" something you can see. `FDA` and `fda` are the same tag; the label keeps what you typed.
  In a vault this is usually denser and more deliberate than the folder tree.

A directory containing `.obsidian/` is marked as a vault. That directory holds UI state rather
than knowledge — the notes hold the knowledge — but it does carry one preference worth honouring:
if the vault's own graph view hides unresolved links, so does this one, until you say otherwise.

See `spore.json`. Part of the first-party Aneural spores; installable from the Open Spores
Marketplace or enabled by default.
