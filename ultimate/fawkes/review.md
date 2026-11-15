# Review of new Ultimate commits for every extension

A host moved to a new Ultimate commit and is now permanent. This review reads what the commits since the last review
change for extensions, and says, with evidence, what each extension on this machine should fix or could use. The
message that sent you here has the facts: the commits to review as a `<range>` for `git log` (the new commit and,
excluded, every commit reviewed before), the base of their diff `<from>..<to>`, the clone, and every extension the host
loads beyond the built-in ones, with its real path and owning project.

Read only. Never check out, merge, pull or reset anything in the clone, and change no code: propose, and wait for the
user's go.

## 1. The commits

In the clone:

```sh
git log --format='%h %s' <range>
```

Note the commits whose subject touches extensions, the host API, events, hooks, actions, settings or packages.

## 2. The extension-facing diff

Only these paths, never the whole diff (it runs to megabytes):

```sh
git diff <from>..<to> -- shared/extension-api.ts server/extensions.ts lib/ docs/extensions/ extensions/README.md \
  'extensions/*/api.ts' 'extensions/*/refs.ts'
```

Use `--stat` first when it is long, then read the files that changed. For each change, note the commit
(`git log --format='%h %s' <range> -- <path>`), what was added, renamed, removed or deprecated, and what it does.

## 3. The extensions

The extensions to check are the ones in the message, plus every project's `.ultimate/extensions` (`ultimate.projects()`
in codemode gives each project's root). Group them by owning project, and start one scout per owner in parallel. Each
scout reads, for each extension of its owner:

- its README;
- what it uses: `host.*` members, `hook(...)`, `defineTool`, `section`, the events it listens to or emits, the actions it
  calls or exports (`ultimate.action`, `host.invoke`), the `ultimate/...` modules it imports, and its `api.ts` and
  `refs.ts`;
- against the changes from step 2.

## 4. What to find

For each extension:

- **Breaks:** a symbol it imports or calls that moved, changed its signature or went away; an event or action it relies
  on that changed. Say what fails (it no longer loads, a type error, a silent change of behaviour).
- **Deprecated:** what still works but is on its way out, with what replaces it.
- **New:** an API, event, hook, action, document or feature it could use, even when it works as it is: one that removes
  code it carries, makes it more robust, or does what it does by hand.

Ground each item in the diff and the extension's file and line. Leave out what you could not tie to a commit.

## 5. The report

One report, one section per owning project (an extension in no project gets a section under its folder), each item:

- the Ultimate commit (`abc1234 subject`);
- the extension and its file;
- the proposal, in one or two sentences;
- its size (a line, an afternoon, a branch of its own), and whether it breaks now.

Start with the breaks. When nothing applies, say what you checked: the range, the paths of step 2 and the extensions.

## 6. After the report

Ask the user which items to build. Once they say which, send each owning project its items as branch prompts with
`ultimate.tell({ project, text })`: one message per project, each item with its commit, file, proposal and how to verify
it. Your own project's items become branches here.
