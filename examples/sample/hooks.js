// The sample app's storage hooks, named in the manifest's [hooks]. Fawkes runs them from the permanent generation's
// worktree, so the code of a generation under test never decides how the storage is copied or restored.
//
//   node hooks.js snapshot <home> <snapshot>   copy app.sqlite into the snapshot, or record that there was none
//   node hooks.js restore <snapshot> <home>    put app.sqlite back the way the snapshot found it
//   node hooks.js copy <home> <test-home>      copy app.sqlite for a test run, while the live app writes to it
//
// A copy is `VACUUM INTO` a file beside the target, then a rename: one read of the database, so the copy is consistent
// and has the rows still in its -wal, which a plain cp would lose. A restore leaves app.sqlite as it was or wholly
// restored, and can run again after it failed. With $HOOKS_FAIL_WHILE set, every hook fails while the file it names
// exists (a test's way to make one fail).
const { DatabaseSync } = require('node:sqlite');
const fs = require('node:fs');
const path = require('node:path');

const FILE = 'app.sqlite';
const COMPANIONS = ['-wal', '-shm', '-journal'];
const [command, from, to] = process.argv.slice(2);

if (process.env.HOOKS_FAIL_WHILE && fs.existsSync(process.env.HOOKS_FAIL_WHILE)) {
  console.error(`${command}: failing while ${process.env.HOOKS_FAIL_WHILE} exists`);
  process.exit(1);
}

const copy = (source, target) => {
  const temp = `${target}.tmp`;
  fs.rmSync(temp, { force: true });
  const db = new DatabaseSync(source);
  try {
    db.exec('PRAGMA busy_timeout = 10000');
    db.exec(`VACUUM INTO '${temp.replaceAll("'", "''")}'`);
  } finally {
    db.close();
  }
  fs.renameSync(temp, target);
};

switch (command) {
  case 'snapshot': {
    const present = fs.existsSync(path.join(from, FILE));
    if (present) copy(path.join(from, FILE), path.join(to, FILE));
    // Written last: a restore trusts the snapshot only through it.
    fs.writeFileSync(path.join(to, 'index.json'), JSON.stringify({ present }));
    break;
  }
  case 'restore': {
    // Throws, before anything is touched, when the snapshot is missing.
    const { present } = JSON.parse(fs.readFileSync(path.join(from, 'index.json'), 'utf8'));
    const live = path.join(to, FILE);
    const temp = `${live}.restore`;
    // The snapshot's copy is a finished file nothing has opened, so a file copy of it is whole.
    if (present) fs.copyFileSync(path.join(from, FILE), temp);
    for (const suffix of COMPANIONS) fs.rmSync(live + suffix, { force: true });
    if (present) fs.renameSync(temp, live);
    else fs.rmSync(live, { force: true });
    break;
  }
  case 'copy':
    if (fs.existsSync(path.join(from, FILE))) copy(path.join(from, FILE), path.join(to, FILE));
    break;
  default:
    console.error('Usage: node hooks.js snapshot <home> <snapshot> | restore <snapshot> <home> | copy <home> <test-home>');
    process.exit(2);
}
